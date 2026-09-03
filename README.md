# grindlog (`ngtwitchtimer`)

Tracks a Twitch streamer's NES speedrun attempts by watching the public
stream and OCR-reading the on-screen LiveSplit timer. Works from the
public broadcast alone. Runs, resets, finishes, and per-act splits are detected by a
state machine and logged to SQLite; optional chat integration announces
finishes and answers viewer commands; a static records site
(https://ng.lexicone.com) renders the whole history — daily heartbeat
timelines, death charts, gold segments, and click-into-run splits.

## About

This started as a way to answer one question about a favourite stream:
*how did the grind actually go today?* Speedrunning is hundreds of resets
for every finish, and the story of a day — how deep the runs got, where they
died, how many attempts the good one took — vanishes as soon as the
broadcast ends. grindlog watches the stream the way a viewer would, reads
the LiveSplit window off the video with tesseract, and keeps every attempt.

It is an independent viewer-side tool: it needs no cooperation from the
streamer, no capture-card access, no LiveSplit server, and no chat presence
(chat is optional). Everything it knows, it read off the public video.
The reference deployment follows **arcus**'s *Ninja Gaiden* (NES) Any%
runs — he's one of the fastest in the world at it — but the game, acts,
layout rectangles and record semantics are all configuration.

What makes it more than a timer scraper:

- **Layout resilience.** The streamer switches OBS scenes, nudges and
  resizes the LiveSplit window. The bot probes configured layouts at a grid
  of pixel offsets, re-anchors on small drifts, and measures the pane's row
  pitch from the frame at every lock, so the splits column and attempt
  counter follow the window instead of a hand-measured rectangle.
- **Capture health.** Every session records how much of the feed it read
  and every layout event, and the site shows it per day — a bad day is
  visible rather than silently thin.
- **Backfill.** Old VODs are streamed straight from Twitch and land on the
  original broadcast timeline, so the history is as deep as the VODs.
- **Speedrun semantics.** Seasonal bests vs lifetime PB, a pre-tracking
  baseline that a "new record" must beat, run identity by the streamer's own
  LiveSplit attempt counter, and a plausibility floor so a frozen timer is
  never a finish.

It is written in Rust with no Python in the toolchain; the only external
programs are `ffmpeg` and `tesseract`.

**Status:** actively used daily; expect rough edges around layouts other
than the ones it was calibrated on — the `locate` and `calibrate` commands
exist to make new ones quick to add.

Where things live in the configuration (see `config.example.toml`, every
field commented with its default):

- `[timer]` — the timer crop; `[splits]` — LiveSplit's cumulative column
  (per-act splits are detected by *change* against the comparison baseline,
  enabling golds, Sum of Best and `!pace`); `[attempts_counter]` — the
  streamer's lifetime attempt counter, which becomes the run's identity;
  `[lifetime_sob]` — the Sum of Best row.
- `[[layouts]]` — the same rectangles for other OBS scenes;
  `[layout_search]` — how far to search when the window moves.
- `[game]` — acts, `record_label`, `baseline_best`, `references`.
- `[stream]` — source (live HLS, VOD, file), quality, `title_filter`,
  `active_hours`, `session_tag`, `recorded_start`/`start_secs` for VODs.
- `[chat]` — bot account, `command_prefix`, mods, announcements.

## Requirements

- **ffmpeg** (video decode; already the only hard runtime dependency)
- **tesseract** for OCR — any of:
  - the `tesseract` CLI binary on `$PATH` (Gentoo: `emerge app-text/tesseract`,
    make sure the `eng` traineddata is installed) — used by the default build;
  - no root? A user-space install works fine (what the reference deployment
    uses): extract the [tesseract AppImage](https://github.com/AlexanderP/tesseract-appimage)
    with `--appimage-extract` into `~/.local/opt/tesseract-appimage/` and put a
    one-line wrapper at `~/.local/bin/tesseract`:
    `exec "$HOME/.local/opt/tesseract-appimage/AppRun" "$@"`;
  - or build with `cargo build --release --features leptess-ocr` for
    in-process OCR (needs libtesseract + libleptonica dev libraries). This
    is **2-3x faster overall**: ~70% of a CLI call is process startup and
    loading the language model, which the in-process engine pays once.
    Without system dev packages you can build against the AppImage's own
    shared libraries: fetch the matching `tesseract` and `leptonica` headers
    from their source releases into `~/.local/opt/ocr-dev/include/`, symlink
    `libtesseract.so.5` / `libleptonica.so.6` (and `libgif.so.7`) from the
    extracted AppImage into `~/.local/opt/ocr-dev/rt/`, write small
    `tesseract.pc` / `lept.pc` files pointing at them, then
    `PKG_CONFIG_PATH=~/.local/opt/ocr-dev/lib/pkgconfig RUSTFLAGS="-C link-arg=-Wl,-rpath,$HOME/.local/opt/ocr-dev/rt" cargo build --release --features leptess-ocr`.
    Never put the whole AppImage `usr/lib` on `LD_LIBRARY_PATH` — it carries
    its own glibc. Set `ocr.engine = "leptess"` and `ocr.tessdata_path` to
    the AppImage's `usr/share/tesseract-ocr/5/tessdata`.
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
ngtwitchtimer                    # watch, detect, log (config.toml in cwd)
ngtwitchtimer -c live.toml run   # any config file
ngtwitchtimer report             # PBs, today's stats, recent runs
ngtwitchtimer report --json      # machine-readable (feeds the site)
ngtwitchtimer locate             # find the LiveSplit pane in a frame
ngtwitchtimer calibrate          # tune the timer crop by eye
RUST_LOG=ngtwitchtimer=debug ngtwitchtimer   # per-frame tracing
```

The bot survives stream drops (auto-restart with backoff) and goes dormant
while the channel is offline, polling every `offline_poll_secs` (120 by
default; with `active_hours` set it polls that often only inside the window
and every `quiet_poll_secs` outside it). `title_filter` skips broadcasts of
other games. All times are stored as `i64` milliseconds in `ngtimer.db`
(tables: `sessions`, `runs`, `splits`, `transitions`, `settings`; sessions
also carry per-broadcast capture health).

## Chat commands

Enable under `[chat]` with a bot account and an IRC OAuth token. `channel`
is where the bot talks (it can differ from the channel it watches — point it
at your own channel to test without posting in the streamer's chat).
`command_prefix` namespaces every command for shared channels: with
`command_prefix = "ngrust-"` the commands below are `!ngrust-pb`,
`!ngrust-status`, and so on, and bare `!pb` is left to other bots.

Viewer commands (shared 10s cooldown — usable by anyone, handy for live
testing without mod rights):

| command | reply |
|---|---|
| `!pb` | the record (season best incl. pre-tracking baseline) + best tracked run |
| `!lastrun` (`!last`) | last run's time, or where it reset |
| `!today` | attempts / finished / resets / best today |
| `!attempts` | total logged attempts |
| `!deaths` (`!resets`) | resets by act |
| `!pace` | last completed act vs record pace, live during a run |
| `!splits` | the current run's completed splits |
| `!golds` (`!gold`) | best segment per act + Sum of Best |
| `!status` (`!timer`, `!ngtimer`) | tracker phase, timer estimate (marked "projected" when OCR is stale), last read, share of frames read this session, locked layout |

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
probing. A lock whose digits sit within a few pixels of the crop's top or
bottom gets the crop enlarged around them (a themed layout's 90px timer in a
100px crop would otherwise read as clipped after any small drift and be
dropped), and a re-anchor needs drift measurements spanning several different
final digits, since digits differ in width and the hundredths digit repeats
for many frames.

**The hundredths font.** LiveSplit draws the fraction of the main timer in a
smaller font, and at stream resolution its decimal point is a couple of
pixels that thresholding erases: `4.76` reads as `476`, `3:06.12` as
`3:06 12`. Every attempt starts in that sub-ten-second range, so the timer's
text — and only the timer's — is parsed leniently (`parse_timer_text`),
which is what makes a reset a few seconds after starting visible at all.
Split rows and reference times stay strict.

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

## Scripts and operations

| script | purpose |
|---|---|
| `scripts/build-release.sh` | build `target/release/ngtwitchtimer`, with the in-process OCR engine when the `~/.local/opt/ocr-dev` toolchain is staged (see Requirements), else the CLI engine |
| `scripts/run-live.sh` | supervise the live bot (restart on exit, log rotation, `logs/live.log`); run it inside tmux |
| `scripts/rollout.sh [--allow-dirty]` | ship a build to the live bot: build, tests, a one-minute smoke replay that must read ≥90%, wait for the bot to be between runs (or offline), restart it through the supervisor, confirm it came back |
| `scripts/replay-window.sh <cfg> <vod_id> <start_secs> <dur_secs> [binary] [label]` | replay one window of a VOD and score the capture against the runner's own attempt counter (runs found, run numbers, lock events); run it with the old and the new binary before trusting any OCR or detection change |
| `scripts/backfill-vods.sh <vod_id>...` | analyze Twitch VODs straight from Twitch, one database each in `backfill-db/`; run several chains in parallel |
| `scripts/import-vod.sh <vod_id> [--deploy]` | replace one broadcast day in the live database from its completed VOD database |
| `scripts/import-when-done.sh <vod_id>...` | detached: import each VOD as its chain finishes and redeploy the site |
| `scripts/merge-backfill.sh [--swap]` | full chronological rebuild from every completed VOD database (stop the bot before `--swap`) |
| `scripts/fill-run-numbers.sh [db]` | infer missing LiveSplit run numbers where the arithmetic between known neighbours is unambiguous |
| `scripts/build-site.sh` / `deploy-site.sh` / `deploy-if-live.sh` | bake `report --json` into `site/index.html`, upload to S3 and invalidate CloudFront; the last one is for a frequent cron that only deploys while the streamer is live |
| `scripts/make-test-video.sh` | synthetic timer video for end-to-end tests |
| `infra/site-stack.yml` | CloudFormation for the site (S3 + CloudFront + certificate + Route53 alias); pass your own `HostedZoneId` |

Typical rhythm: the live bot runs all day in tmux; a cron deploys the site
every few minutes while live and once nightly; old VODs are backfilled in
three chains with `import-when-done.sh` landing each day as it completes.

## Maintenance notes

- Twitch occasionally rotates the web player client-id or retires the GraphQL
  persisted query. Symptoms: GQL 400s in the log. Fix: see the comments at
  the top of `src/twitch_hls.rs` (one-line updates, current values are in
  streamlink's `twitch.py`).
- When the streamer starts a new season (LiveSplit comparison reset), update
  `game.baseline_best` — his comparison column is the season best, not the
  lifetime PB.
- tesseract is run with `OMP_THREAD_LIMIT=1`: on these small crops one thread
  is faster per call, and several workers sharing the cores no longer
  spin-wait each other to a crawl.
- `NG_DUMP_PANE=1` makes the lock-time pane analysis save what it saw to
  `calibration/pane.png` and log every word it read — the first thing to
  look at when a new layout reads its splits as blanks.
- Sessions record capture health; a day whose "capture" line on the site
  reads far below ~90% is the cue to run `locate` against that VOD.

## Contributing

Issues and pull requests are welcome. The best first contributions are
layout configurations for other streamers/games (with a `locate` printout
and a frame), detection edge cases with an `obs_log` excerpt, and site
ideas. Please keep the toolchain Rust + ffmpeg + tesseract only.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

This project is not affiliated with Twitch, LiveSplit, or the streamers it
follows. It only reads publicly broadcast video.
