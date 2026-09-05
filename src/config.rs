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
    /// The reference rows under the timer. Set the rectangle to span them
    /// all, even if only one matters: only what the configured rectangles
    /// cover is decoded, and the pane pass (once a minute while a layout is
    /// locked) reads every labelled reference time it finds there (Sum of
    /// Best, the season best labelled with a year, PB, WR). Only when it
    /// labels nothing is this crop OCR'd on its own as LiveSplit's "Sum of
    /// Best Segments" row (seasonal, given his splits-file practice).
    #[serde(default)]
    pub lifetime_sob: CounterCfg,
    /// Alternate on-screen layouts (other OBS scenes). The base sections above
    /// are layout 0; the bot probes every layout's timer until one parses
    /// consistently, locks to it, and re-probes if the timer goes dark, parses
    /// on fewer than 40% of 60 read frames, or reads clipped by the crop edge
    /// ten frames in a row.
    #[serde(default)]
    pub layouts: Vec<LayoutCfg>,
    #[serde(default)]
    pub layout_search: LayoutSearchCfg,
    /// Other games the layout's title row may name (`[[games]]`), for the
    /// board reader (`game.follow_title`): a title that fuzzy-matches none
    /// of `game.name` is looked up here by substring, so "Randomized
    /// Arcathlon" and "Arcathlon #6" file under one name.
    #[serde(default)]
    pub games: Vec<GameAlias>,
}

/// What the board reader does with the game the pane's title row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FollowTitle {
    /// Nothing: the title is logged and recorded as it always was.
    #[default]
    Off,
    /// Shadow mode: log what the reader would file the board under and
    /// the rows it read, and record a `layout` session event for every
    /// distinct board; nothing changes in what is recorded.
    Log,
}

/// A `[[games]]` entry: the name (and category) a title is filed under
/// when the normalised title contains one of the `match` strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameAlias {
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    /// Lowercase substrings of the normalised title (letters, digits and
    /// single spaces) that identify the game; any one matching is enough.
    #[serde(rename = "match", default)]
    pub r#match: Vec<String>,
}

/// Tolerance for the streamer nudging the LiveSplit window a few pixels.
/// When the locked timer goes dark, nearby pixel offsets are probed and, if
/// one parses consistently, the whole layout is re-anchored at that offset.
/// While locked, a consistent shift of the digits' ink over eight seconds
/// (hits spanning at least three different final digits) re-anchors the
/// layout without dropping the lock, provided every crop stays inside the
/// decoded union, which `drift_px + step_px` pads. `drift_px = 0` disables
/// both the offset search and this fine-drift re-anchor.
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

/// A crop with threshold/invert settings for a static text row of the
/// layout: `[attempts_counter]`, and reused for `[lifetime_sob]`.
///
/// `[attempts_counter]` is LiveSplit's lifetime attempt counter (the number
/// in the layout header). It is OCR'd every 2 s while the current run has no
/// number yet and the timer was accepted within the last 3 s, and stored on
/// the run row once `counter::CounterTracker` accepts the value — correlates
/// our per-category numbering with the runner's own lifetime count.
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
    /// Append one JSON line per analyzed frame to this file: timer text
    /// (`ocr`, from whichever reader read it), parsed value, phase, events,
    /// active layout and offset, ink and anchor positions, static flag, the
    /// retry threshold that rescued the read, which reader read it, and the
    /// splits when they were read. Invaluable for tuning detection against a
    /// VOD; replayable and `tail -f`-able. With NG_DUMP_TIMER set, the raw
    /// timer crop of every 25th frame (every frame with NG_DUMP_TIMER=all) is
    /// saved beside this file under calibration/timer-<frame>.png; log and
    /// crops together are a corpus for `glyphs train` and `glyphs test`.
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
    /// Frames per second fed to the timer reader. The `[detection]` counts
    /// (start/stall/reset/desync confirmations, illegible_reset_count) are
    /// consecutive frames, so at 2 fps each covers half the wall-clock time
    /// it does at 1, which the defaults are tuned for; the drift re-anchor
    /// rule is in seconds and scales itself. live.toml runs 2.
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
        // Empty means unset, as the example config documents it.
        match self.recorded_start.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
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
    /// Choose the timer's threshold from each crop's own histogram (Otsu)
    /// instead of the fixed value above. Themes differ in how bright the
    /// timer is; measured on this stream, the fixed 60 that reads one theme
    /// at 97% reads another at 92% while 75 does the reverse (98% / 56%).
    #[serde(default)]
    pub auto_threshold: bool,
    /// When a locked timer read does not parse, re-threshold the same crop at
    /// each of these (fixed value, no Otsu) and read again with tesseract,
    /// whichever reader is configured; the first that parses wins. A rescued
    /// value is trusted only where it agrees with the running clock (within
    /// detection.max_jump_ms), so a rescued fragment cannot start or restart
    /// a run by itself. Costs OCR only on frames that already failed.
    /// Measured on this stream, the themes want different cutoffs (60 vs
    /// 75), and this covers both.
    #[serde(default)]
    pub retry_thresholds: Vec<u8>,
    /// How the timer's digits are read: "tesseract" (general OCR), or
    /// "glyph" — a purpose-built reader matching each glyph against templates
    /// harvested from this streamer's own footage (`ngtwitchtimer glyphs
    /// train`), with tesseract as the fallback when it declines a frame.
    #[serde(default = "d_reader")]
    pub reader: String,
    /// Template file for the glyph reader.
    #[serde(default = "d_glyph_templates")]
    pub glyph_templates: String,
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
            reader: d_reader(),
            glyph_templates: d_glyph_templates(),
            auto_threshold: false,
            retry_thresholds: Vec::new(),
            invert: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrCfg {
    /// "auto" (leptess if compiled in and it initializes, e.g. finds its
    /// tessdata; otherwise cli, with a warning), "cli", or "leptess".
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
    /// Once the pane pass has read the season best the layout prints under
    /// the timer (see `lifetime_sob`), that value replaces this one and is
    /// remembered across restarts, so a season rollover needs no edit here.
    #[serde(default)]
    pub baseline_best: Option<String>,
    /// Only record runs while the layout's own title row names this game.
    /// Lets a marathon broadcast be captured, recording just the segments
    /// that are actually this game. Off by default: a misread title would
    /// otherwise stop recording.
    #[serde(default)]
    pub require_title_match: bool,
    /// The board reader: "off" (default) or "log" — read the whole pane
    /// (title, every split row with its name and times) at each pane pass
    /// and log which game it would file the board under, recording a
    /// `layout` session event per distinct board. Shadow mode: nothing
    /// acts on it yet. Exclusive with `require_title_match`.
    #[serde(default)]
    pub follow_title: FollowTitle,
    /// Publish each session's Twitch VOD id (and so a "watch" link for every
    /// run) in the report and the site. A VOD id names the channel; off by
    /// default, like the page, which names the game only.
    #[serde(default)]
    pub public_vod_links: bool,
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
            follow_title: FollowTitle::Off,
            public_vod_links: false,
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
        if self.game.follow_title != FollowTitle::Off && self.game.require_title_match {
            bail!(
                "game.follow_title = {:?} with game.require_title_match = true: \
                 following the title and suspending on it are exclusive",
                self.game.follow_title
            );
        }
        for g in &self.games {
            if g.name.trim().is_empty() {
                bail!("a [[games]] entry has no name");
            }
            if !g.r#match.iter().any(|m| !m.trim().is_empty()) {
                bail!(
                    "[[games]] entry {:?} has no match strings, so no title can ever name it",
                    g.name
                );
            }
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
fn d_reader() -> String {
    "tesseract".into()
}
fn d_glyph_templates() -> String {
    "assets/glyphs.json".into()
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

    /// The two configs in the repository parse: the example everyone copies,
    /// and the live one the deployed bot reads.
    #[test]
    fn repository_configs_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        Config::load(&root.join("config.example.toml")).expect("config.example.toml");
        Config::load(&root.join("live.toml")).expect("live.toml");
    }

    /// Every commented-out `# field = value` in the example is a real field
    /// with a value that parses and validates: with them all switched on, the
    /// example still loads. Unknown fields are rejected, so a field that was
    /// renamed or removed fails here rather than in a user's config.
    #[test]
    fn example_config_documents_only_real_fields() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join("config.example.toml")).unwrap();
        let mut on = String::new();
        for line in text.lines() {
            // "# key = value   # explanation" -> "key = value"; prose
            // comments and continuation lines stay comments.
            let uncommented = line
                .strip_prefix("# ")
                .filter(|rest| {
                    let key = rest.split('=').next().unwrap_or("").trim();
                    rest.contains(" = ")
                        && !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '[' || c == ']')
                })
                .map(|rest| rest.split("  #").next().unwrap_or(rest).trim_end());
            match uncommented {
                Some(l) => on.push_str(l),
                None => on.push_str(line),
            }
            on.push('\n');
        }
        // The commented layout block: its header and rectangles too.
        let on = on.replace("# [[layouts]]", "[[layouts]]");
        let cfg = parse(&on).unwrap_or_else(|e| panic!("example with every field on: {e:#}\n{on}"));
        assert_eq!(cfg.timer.reader, "tesseract");
        assert_eq!(cfg.layouts.len(), 1);
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
    fn board_reader_settings_parse_and_exclude_the_title_gate() {
        let cfg = parse(
            "[stream]\nchannel = \"x\"\n[game]\nfollow_title = \"log\"\n\
             [[games]]\nname = \"Arcathlon\"\nmatch = [\"arcath\"]\ncategory = \"10 games\"\n\
             [[games]]\nname = \"Mega Man 2\"\nmatch = [\"mega man 2\", \"mm2\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.game.follow_title, FollowTitle::Log);
        assert_eq!(cfg.games.len(), 2);
        assert_eq!(cfg.games[0].category.as_deref(), Some("10 games"));
        assert_eq!(cfg.games[1].r#match, ["mega man 2", "mm2"]);
        assert_eq!(cfg.games[1].category, None);
        // The default is off, with no aliases.
        let cfg = parse("[stream]\nchannel = \"x\"\n").unwrap();
        assert_eq!(cfg.game.follow_title, FollowTitle::Off);
        assert!(cfg.games.is_empty());
        // Following the title and suspending on it are exclusive.
        let err = parse(
            "[stream]\nchannel = \"x\"\n[game]\nfollow_title = \"log\"\nrequire_title_match = true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("exclusive"), "{err}");
        // Only the values this version knows.
        assert!(parse("[stream]\nchannel = \"x\"\n[game]\nfollow_title = \"on\"\n").is_err());
        // An alias nothing can match is a mistake.
        assert!(parse("[stream]\nchannel = \"x\"\n[[games]]\nname = \"Arcathlon\"\n").is_err());
        assert!(
            parse("[stream]\nchannel = \"x\"\n[[games]]\nname = \"\"\nmatch = [\"a\"]\n").is_err()
        );
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
