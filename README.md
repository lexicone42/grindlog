# ngtwitchtimer

Tracks a Twitch streamer's NES speedrun attempts by watching the public
stream and OCR-reading the on-screen LiveSplit timer. Works from the
public broadcast alone. Runs, resets, finishes, and per-act splits are detected by a
state machine and logged to SQLite; optional chat integration announces
finishes and answers viewer commands; a static records site
(https://ng.lexicone.com) renders the whole history — daily heartbeat
timelines, death charts, gold segments, and click-into-run splits.

Highlights beyond the basics:

- **Splits-panel OCR**: a second crop over LiveSplit's cumulative column
  detects per-act splits by *change* against the comparison baseline
  (tie-backfill included), enabling golds, Sum of Best, and `!pace`.
- **Sessions**: one row per broadcast; recorded sources land on the original
  broadcast timeline (`recorded_start`, auto-fetched for VODs).
- **Record semantics**: `record_label` ("season best"), `baseline_best` (a
  pre-tracking best that NEW-record announcements must beat), and
  `references` (WR / lifetime PB shown on the site).
- **Title gate**: `stream.title_filter` skips broadcasts of other games.
- **Backfill**: `scripts/backfill-vods.sh <ids...>` streams old VODs straight
  from Twitch into a rebuild database, chronologically.
- **Site**: `scripts/build-site.sh` bakes `report --json` into one
  self-contained HTML page; `scripts/deploy-site.sh` pushes it to
  S3+CloudFront (`infra/site-stack.yml`); a nightly cron keeps it fresh.
- **Ops**: run supervised in tmux via `scripts/run-live.sh` (auto-restart,
  logs to `logs/live.log`).

## Requirements

- **ffmpeg** (video decode; already the only hard runtime dependency)
- **tesseract** for OCR — any of:
  - the `tesseract` CLI binary on `$PATH` (Gentoo: `emerge app-text/tesseract`,
    make sure the `eng` traineddata is installed) — used by the default build;
  - no root? A user-space install works fine (this is what's set up on this
    machine): extract the [tesseract AppImage](https://github.com/AlexanderP/tesseract-appimage)
    with `--appimage-extract` into `~/.local/opt/tesseract-appimage/` and put a
    one-line wrapper at `~/.local/bin/tesseract`:
    `exec "$HOME/.local/opt/tesseract-appimage/AppRun" "$@"`;
  - or build with `cargo build --release --features leptess-ocr` for
    in-process OCR (needs libtesseract + libleptonica dev libraries).
- *(optional)* **streamlink**, only if you set `source = "streamlink"`.
  The default `hls` source resolves the stream URL itself in Rust.

Everything else (SQLite, HLS resolution, Twitch chat) is compiled in.

## Setup

```sh
cargo build --release
cp config.example.toml config.toml   # edit: channel, crop rectangle
```

### Calibrating the crop rectangle

1. `ngtwitchtimer calibrate --full-frame` — while the stream is live, saves
   `calibration/full.png` (scaled to the 1920x1080 canvas). Open it, note the
   timer's x/y/w/h, put them under `[timer]` in config.toml.
2. `ngtwitchtimer calibrate` — saves `calibration/crop.png` (raw crop) and
   `calibration/processed.png` (what tesseract sees: should be clean black
   digits on white), and prints live OCR readings. Tune `threshold` /
   `invert` / the crop until readings parse cleanly.

### Finding the LiveSplit pane automatically

`ngtwitchtimer locate` OCRs a whole frame (from the configured source, or
`--image some-frame.png`) and picks out the time-shaped words: the big timer,
the split rows above it, the attempt counter, the "Sum of Best" row. It
prints a ready-to-paste `[[layouts]]` entry, says how far the pane sits from
each configured layout (`offset +18,+12 px`, `digits CLIPPED`), and draws the
boxes into `calibration/locate.png`. Use it to add a new OBS scene as a
layout, or to check whether the streamer moved the window. With
`source = "vod"`, `stream.start_secs = 7200` seeks two hours in.

### Testing against a VOD or file first (recommended)

Detection can be tested reproducibly before pointing at a live stream:

```toml
[stream]
source = "vod"            # or "file" with input = "clip.mp4"
vod_id = "2861394875"     # twitch.tv/videos/<id>
[debug]
obs_log = "observations.jsonl"
```

Recorded sources tick the state machine by frame index instead of wall
clock, so processing speed doesn't affect detection and results are
deterministic. The obs log has one JSON line per frame (OCR text, parsed
value, phase, events) — `tail -f` it or grep for `"events": [` to see every
decision.

`scripts/make-test-video.sh` generates a synthetic session (run → finish →
reset) with ffmpeg only; the expected detections are in its header comment.

## Running

```sh
ngtwitchtimer                # watch, detect, log (config.toml in cwd)
ngtwitchtimer report         # PBs, today's stats, recent runs
ngtwitchtimer report --json  # machine-readable (e.g. for a records website)
RUST_LOG=ngtwitchtimer=debug ngtwitchtimer   # per-frame tracing
```

The bot survives stream drops (auto-restart with backoff) and goes dormant
while the channel is offline, polling every 2 minutes. All times are stored
as `i64` milliseconds in `ngtimer.db` (tables: `runs`, `transitions`,
`settings`).

## Chat commands

Enable under `[chat]` with a bot account and an IRC OAuth token.

Viewer commands (shared 10s cooldown — usable by anyone, handy for live
testing without mod rights):

| command | reply |
|---|---|
| `!pb` | the record (season best incl. pre-tracking baseline) + best tracked run |
| `!lastrun` | last run's time, or where it reset |
| `!today` | attempts / finished / resets / best today |
| `!attempts` | total logged attempts |
| `!deaths` (`!resets`) | deaths by act |
| `!pace` | last completed act vs record pace, live during a run |
| `!splits` | the current run's completed splits |
| `!golds` | best segment per act + Sum of Best |
| `!status` (`!timer`, `!ngtimer`) | tracker phase, timer estimate (marked "projected" when OCR is stale), last read |

Mod commands (broadcaster, badge mods, or logins listed in `chat.mods`):

| command | effect |
|---|---|
| `!setgame <game...> <category>` | switch tracked game (last word = category); persisted |
| `!correct <time>` | fix the last run's final time (e.g. `!correct 12:34.5`) |
| `!void` | delete the last logged run |

Finished runs are announced automatically (with PB callout) when
`chat.announce = true`.

## How detection works

1 fps frames → crop → 4x upscale + threshold → tesseract with a `0123456789:.`
whitelist → parsed to ms → state machine:

- **IDLE → RUNNING**: timer leaves ~0:00 and advances consistently for 3
  readings (also fires when joining mid-run; start time is back-dated by the
  timer value).
- **RUNNING → FINISHED**: legible but frozen value for 5 consecutive
  readings; the frozen value is the final time.
- **RUNNING → RESET**: timer back at ~zero (2 readings), OCR dead for
  ~3 minutes while frames still arrive (`disappeared` = DNF), or a sustained
  desync (missed reset → close old run, re-sync onto the new one).

Misreads are rejected against the *wall clock*: a reading is accepted only if
it advanced by roughly the elapsed time since the last good reading (±5s), so
stream drops and ad breaks self-heal instead of poisoning the state.

**Layouts and drift.** Streamers switch OBS scenes and nudge the LiveSplit
window. Every `[[layouts]]` entry is a set of rectangles for one scene; the
bot probes each layout's timer (and, around it, a grid of pixel offsets up to
`layout_search.drift_px`) and locks to the first position that parses
consistently on five looks — frozen, or advancing with the clock. A locked
position that goes unreadable for `dark_frames_search` frames starts the
probe again, so a scene switch or a few-pixel nudge is picked up and logged
(`layout switched …` / `re-anchored: LiveSplit moved +18,+12 px`) rather than
silently losing runs. The union of every rectangle (plus the drift margin)
is the only crop ffmpeg decodes, so extra layouts cost OCR calls only while
probing.

**Pane geometry.** A resized LiveSplit window changes the row pitch, which
no shift of the configured rectangles can follow, so at every lock the bot
measures the pane itself: one sparse-text OCR pass over the decoded crop
finds the time-shaped words above the timer, groups them into rows, takes
the median pitch and the right-aligned cumulative column, and derives the
splits rectangle (and the attempt counter above it) from that. The
configured `splits`/`attempts_counter` rectangles are the fallback when
fewer than two rows can be read (`pane geometry: 6/6 split rows read, pitch
45px; …` in the log). Layouts whose timer rectangles overlap are told apart
the same way: the one whose splits column reads as times wins.

## Maintenance notes

- Twitch occasionally rotates the web player client-id or retires the GraphQL
  persisted query. Symptoms: GQL 400s in the log. Fix: see the comments at
  the top of `src/twitch_hls.rs` (one-line updates, current values are in
  streamlink's `twitch.py`).
