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
the LiveSplit window off the video (tesseract, or for the timer a template
reader trained on the streamer's own footage), and keeps every attempt.

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

It is written in Rust with no Python in the toolchain; the bot's only
external programs are `ffmpeg` and `tesseract`. The helper scripts under
`scripts/` also use `sqlite3`, `curl`, `jq`, `flock` and, for the site
deploy, the AWS CLI (see Scripts and operations).

**Status:** actively used daily; expect rough edges around layouts other
than the ones it was calibrated on — the `locate` and `calibrate` commands
exist to make new ones quick to add.

Where things live in the configuration (see `config.example.toml`, every
field commented with its default):

- `[timer]` — the timer crop, its `threshold`, `retry_thresholds` (cutoffs
  a failed read is retried at) and `reader` (`"tesseract"` or `"glyph"`, see
  Setup); `[splits]` — LiveSplit's cumulative column (per-act splits are
  detected by *change* against the comparison baseline, enabling golds, Sum
  of Best and `!pace`); `[attempts_counter]` — the streamer's lifetime
  attempt counter, which becomes the run's identity; `[lifetime_sob]` — the
  rows under the timer, where every labelled reference time (Sum of Best,
  the year-labelled season best, PB, WR) is read at each lock; a season best
  read there replaces `baseline_best` as the record to beat.
- `[[layouts]]` — the same rectangles for other OBS scenes;
  `[layout_search]` — how far to search when the window moves.
- `[game]` — acts, `record_label`, `baseline_best`, `references`,
  `require_title_match` (record only while the layout's title row names the
  game).
- `[detection]` — the state machine's confirmation counts and tolerances,
  in frames; `min_final_ms` is the plausibility floor below which a frozen
  timer is never a finish.
- `[stream]` — source (live HLS, VOD, file), quality, `fps`, `title_filter`,
  `active_hours`, `session_tag`, `recorded_start`/`start_secs` for VODs.
- `[ocr]` — `engine` (`"auto"`, `"cli"`, `"leptess"`), `tesseract_cmd`,
  `tessdata_path`.
- `[chat]` — bot account, `command_prefix`, mods, announcements.
- `[debug]` — `obs_log`, one JSON line per analysed frame.

## Requirements

- **ffmpeg** (video decode)
- **tesseract** for OCR — any of the following. The in-process build does
  not fully replace the CLI: `locate` always shells out to it, and the
  default `ocr.engine = "auto"` falls back to it when the in-process engine
  cannot start. A binary off `$PATH` can be named with `ocr.tesseract_cmd`
  (default `"tesseract"`) instead of a wrapper script.
  - the `tesseract` CLI binary on `$PATH` (Gentoo: `emerge app-text/tesseract`,
    make sure the `eng` traineddata is installed) — used by the default build;
  - no root? A user-space install works fine (what the reference deployment
    uses): extract the [tesseract AppImage](https://github.com/AlexanderP/tesseract-appimage)
    with `--appimage-extract` into `~/.local/opt/tesseract-appimage/` and put a
    one-line wrapper at `~/.local/bin/tesseract`:
    `exec "$HOME/.local/opt/tesseract-appimage/AppRun" "$@"`;
  - or build with `cargo build --release --features leptess-ocr` for
    in-process OCR (needs libtesseract + libleptonica dev libraries). This
    is faster overall (the one measurement, on a VOD window: 669 to 964
    frames per 100 s of video, about 1.4x): ~70% of a CLI call is process
    startup and loading the language model, which the in-process engine
    pays once.
    Without system dev packages you can build against the AppImage's own
    shared libraries: fetch the matching `tesseract` and `leptonica` headers
    from their source releases into `~/.local/opt/ocr-dev/include/`, symlink
    `libtesseract.so.5` / `libleptonica.so.6` (and `libgif.so.7`) from the
    extracted AppImage into `~/.local/opt/ocr-dev/rt/`, write small
    `tesseract.pc` / `lept.pc` files pointing at them, then
    `PKG_CONFIG_PATH=~/.local/opt/ocr-dev/lib/pkgconfig RUSTFLAGS="-C link-arg=-Wl,-rpath,$HOME/.local/opt/ocr-dev/rt" cargo build --release --features leptess-ocr`.
    `scripts/build-release.sh` runs exactly this (plus
    `BINDGEN_EXTRA_CLANG_ARGS` pointing at the staged headers) whenever
    `~/.local/opt/ocr-dev`, or the directory named in `$OCR_DEV`, holds
    `lib/pkgconfig/tesseract.pc` and `rt/libtesseract.so.5`, and falls back
    to a plain `cargo build --release` (CLI engine) otherwise; extra
    arguments go through to cargo. It is what `scripts/rollout.sh` and the
    live deployment build with.
    Never put the whole AppImage `usr/lib` on `LD_LIBRARY_PATH` — it carries
    its own glibc. Set `ocr.tessdata_path` to the AppImage's
    `usr/share/tesseract-ocr/5/tessdata` and leave `ocr.engine` at its
    default `"auto"` (what the reference deployment uses): it picks the
    in-process engine when the binary has it and falls back to the CLI with
    a warning when it cannot start. `ocr.engine = "leptess"` makes that
    failure fatal instead.
- *(optional)* **streamlink**, only if you set `source = "streamlink"`.
  The default `hls` source resolves the stream URL itself in Rust.

Everything else (SQLite, HLS resolution, Twitch chat) is compiled in.

## Setup

```sh
cargo build --release
cp config.example.toml config.toml   # edit: channel, crop rectangle
```

### Calibrating the crop rectangle

1. `ngtwitchtimer calibrate --full-frame` — with the stream live (or
   `[stream] source` pointed at a VOD or file, see below), saves
   `calibration/full.png` (scaled to the 1920x1080 canvas). Open it, note the
   timer's x/y/w/h, put them under `[timer]` in config.toml.
2. `ngtwitchtimer calibrate` — saves `calibration/crop.png` (raw crop) and
   `calibration/processed.png` (what tesseract sees: should be clean black
   digits on white), and prints live tesseract readings. Tune `threshold` /
   `invert` / the crop until readings parse cleanly. With `reader = "glyph"`
   the bot reads the raw crop instead: `ngtwitchtimer glyphs boxes
   calibration/crop.png` shows what the template reader makes of it.

### Reading the timer with templates instead of tesseract

The timer is one font at one size, and a general-purpose OCR engine is the
wrong tool for it: over ~15,000 labelled frames of the reference channel,
tesseract misreads the small hundredths digits on 2-4% of frames ("11" as
"14", "77" as "71") and fails outright on a few more. `reader = "glyph"`
under `[timer]` switches the timer to a purpose-built reader: it cuts the
crop into glyphs at empty columns (cutting touching glyphs where the
templates agree on both halves), frames each in its place in the digit band
and matches it against templates harvested from the streamer's own footage
by normalised correlation. Anything uncertain it declines, and tesseract
reads that frame instead — so tesseract stays a requirement, also for the
splits and counter crops. (One exception: while the layout is still being
probed, a position with nothing glyph-shaped in it at all, no digit band or
ink in far too many pieces, is skipped without tesseract on light-on-dark
themes, `invert = true`; glyph-shaped ink the templates do not know still
goes to tesseract, so an unknown theme can still lock.) On the same frames
it reads 99% with no verified error, in about 2 ms a frame rather than 100+.

The templates ship in `assets/glyphs.json`, trained on the reference
channel's two themes. To retrain for another font, size or theme:

1. Replay a VOD window with every locked crop saved, reading the timer with
   tesseract: `reader = "tesseract"` under `[timer]` in the replay config,
   not a copy of `live.toml`, which uses the glyph reader. Labels are
   tesseract's readings; frames the glyph reader read are skipped when the
   corpus is loaded, so templates never learn from the reader's own output,
   and a replay made with `reader = "glyph"` ends in `glyphs train`
   reporting "no confirmed frames in the corpus". The simplest way is
   `scripts/replay-window.sh` with the variable in its environment:
   `NG_DUMP_TIMER=all scripts/replay-window.sh replay.toml <vod_id> <start_secs> <dur_secs>`
   leaves `replays/<label>/obs.jsonl` and, beside it,
   `replays/<label>/calibration/timer-<frame>.png` for every locked frame
   that was OCR'd (a frame identical to the previous one is not). That
   directory is a corpus: `glyphs train` reads `<dir>/obs.jsonl` (the name
   is fixed) and `<dir>/calibration/timer-<frame>.png`. Without the script,
   set `[debug] obs_log = "<dir>/obs.jsonl"` and run
   `NG_DUMP_TIMER=all ngtwitchtimer --config replay.toml run`; the crops land
   beside the log, or in `./calibration/` when the log path has no
   directory. A frame labels its glyphs only when tesseract's reading,
   trimmed to two fraction digits, matches what the tracker accepted within
   150 ms, at the primary threshold, inside a run.
2. `ngtwitchtimer --config replay.toml glyphs train corpus-a corpus-b --out assets/glyphs.json`
   harvests templates from those frames (a few thousand per theme is plenty;
   `--per-class`, default 24, bounds the templates kept per character). The
   config supplies `[timer] threshold`, the ink level the crops are cut at:
   use the one the bot will read with. It is recorded in the template file,
   and a reader that loads the file at another threshold warns that glyphs
   may segment differently. `glyphs test` and `glyphs boxes` take the
   threshold from the config the same way.
3. `ngtwitchtimer glyphs test held-out-corpus --templates assets/glyphs.json`
   scores a corpus the templates were not trained on: right, declined (by
   reason, with an example each) and disagreeing frames against tesseract's
   labels, plus the margin and score distributions of the right readings,
   which is how the reader's decision floors (0.55 score, 0.12 margin) were
   set; `--min-score N --min-margin N`, given together, try other floors.
   `--dump-wrong dir` saves the disagreements and the first two dozen
   declines to look at (most disagreements turn out to be tesseract's), and
   `ngtwitchtimer glyphs boxes crop.png` shows how one crop is cut and
   scored.

### Finding the LiveSplit pane automatically

`ngtwitchtimer locate` OCRs a whole frame (from the configured source, or
`--image some-frame.png`, resized to the canvas if needed) and picks out the
time-shaped words: the big timer, the split rows above it, the attempt
counter, the "Sum of Best" row. It prints a ready-to-paste `[[layouts]]`
entry, says how far the pane sits from each configured layout (`offset
+18,+12 px`, `digits CLIPPED`), and draws the boxes into
`calibration/locate.png`. Use it to add a new OBS scene as a layout, or to
check whether the streamer moved the window. With `source = "vod"`,
`stream.start_secs = 7200` seeks two hours in. It always drives the
`tesseract` CLI (`ocr.tesseract_cmd`), whatever `ocr.engine` says. From a
source it looks at one frame every five seconds and gives up after
`--frames` (12) without a timer, leaving the last one in
`calibration/locate-last.png`.

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
value, phase, events) — `tail -f` it, or `grep '"events":\["'` it for the
frames that carry a decision (the lines are compact JSON, and most hold an
empty `"events":[]`).

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
ngtwitchtimer glyphs train|test|boxes   # the timer's template reader (above)
RUST_LOG=ngtwitchtimer=debug ngtwitchtimer   # per-frame tracing
```

For a deployment that should outlive the terminal, `scripts/run-live.sh`
supervises the bot under tmux and `scripts/rollout.sh` ships a new build to
it (see Scripts and operations).

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

Viewer commands (usable by anyone, handy for live testing without mod
rights; each command has its own cooldown shared by all viewers,
`chat.command_cooldown_secs`, 10 s by default):

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

Mod commands from anyone else are dropped silently, and mod commands skip the
cooldown. `!correct` also marks the run finished (clearing any reset reason),
so it turns a run logged as a reset into a finish; `!setgame` switches
tracking immediately and survives restarts (stored in the `settings` table).

Replies name a run the way the finish announcement and the site do: by the
runner's own LiveSplit counter (`run 96677`) when it was read, and otherwise
by the bot's ordinal marked as such (`tracked #2056`). "The last run" for
`!lastrun`, `!correct` and `!void` is the run of the tracked game and
category that started last, not the last row written: a VOD import lands
older days after newer ones, so the newest row can be weeks old.

Finished runs are announced automatically (with PB callout) when
`chat.announce = true`.

## How detection works

Frames at `stream.fps` (1 by default; the live config runs 2) → the union
crop, decoded in colour, the timer taken from its brightest channel → the
glyph reader when `[timer] reader = "glyph"` (see *Reading the timer with
templates*), and for the frames it declines, or always with
`reader = "tesseract"`, 4x upscale + threshold → tesseract with a
`0123456789:.` whitelist, re-read at each `retry_thresholds` cutoff when
that did not parse → parsed to ms → state machine:

- **IDLE → RUNNING**: timer leaves ~0:00 and advances consistently for 3
  readings (also fires when joining mid-run; start time is back-dated by the
  timer value).
- **RUNNING → FINISHED**: legible but frozen value for 5 consecutive
  readings; the frozen value is the final time. A value frozen below
  `min_final_ms` (11:00 in the live config: nobody finishes NG Any% faster)
  is logged as a reset with reason `tooshort`, so a stream stall or a pause
  mid-run never becomes a PB.
- **RUNNING → RESET**: timer back at ~zero for 2 readings (under
  `reset_epsilon_ms`, or LiveSplit's pre-start `-5.00`, read as a bare
  `5.00`, frozen while the run was well past it); OCR dead for
  `illegible_reset_count` frames while frames still arrive (`disappeared` =
  DNF; the default 180 is three minutes at 1 fps, 90 s at the live config's
  2 fps); or a sustained desync: 3 rejected readings that agree with *each
  other* mean the tracker's baseline is wrong, not the OCR. A value under
  `desync_restart_max_ms` (90 s) or under half the run's last good time is
  a missed reset-and-restart (close the old run, start a new one from the
  reading); anything else is the stream's clock slipping (CDN rewind,
  dropout) and the same run continues re-anchored (`stream clock slipped`
  in the log).

Every count above is in frames: the `[detection]` defaults in
`config.example.toml` assume `stream.fps = 1`, so at 2 fps each is met in
half the wall time.

Misreads are rejected against the *wall clock*: a reading is accepted only if
it advanced by roughly the elapsed time since the last good reading (±5s), so
stream drops and ad breaks self-heal instead of poisoning the state. Three
kinds of OCR noise are recognised before they can count as evidence of a
desync: a reading one confusable glyph away from the expected value (a red
`7:22` reading `1:22` for frames on end; only once both are past a minute,
since a `6:03`/`0:03` pair is far likelier a fast restart); a reading that is
the expected value with its leading digits lost (`1:56.71` read as `6.71` or
`56.71`), set aside for at most two frames and only when no reading under six
seconds, the countdown or zero a real restart passes through, was seen in
the last 15 s; and a reading rescued at one of the `[timer]
retry_thresholds` fallbacks, which is trusted only within `max_jump_ms`
(±5 s) of the running clock.

**Layouts and drift.** Streamers switch OBS scenes and nudge the LiveSplit
window. Every `[[layouts]]` entry is a set of rectangles for one scene; the
bot probes each layout's timer at its configured position on every probe
frame and, taking turns, at a grid of pixel offsets up to
`layout_search.drift_px` (one offset per layout per frame, two for the layout
last locked). A position that parses consistently — frozen, or advancing
with the clock — on five looks becomes the lock (ten when it belongs to a
different layout than the last lock, so overlapping rectangles cannot steal a
scene); digits touching the crop edge disqualify a position however well
they parse, and with the glyph reader a probe position with nothing
glyph-shaped in it is not sent to tesseract at all (see *Reading the timer
with templates*). On an unchanged frame the probe is skipped, at most three
frames in a row and never during a run. A locked position that goes
unreadable for `dark_frames_search` frames, parses under 40% of 60 read
frames, or reads clipped ten times in a row starts the probe again, so a
scene switch or a few-pixel nudge is picked up and logged (`layout switched
…` / `re-anchored: LiveSplit moved +18,+12 px`) rather than silently losing
runs. The union of every rectangle (plus the drift margin) is the only crop
ffmpeg decodes, so extra layouts cost OCR calls only while probing. The
digits' extent is measured as the band of rows holding the most ink, so a
separator line or the row of text under the timer cannot make them look
clipped. A re-anchor needs eight seconds of agreeing measurements (8 ×
`stream.fps` frames, each within 2 px of the last) spanning at least three
different final digits, since digits differ in width and the hundredths
digit repeats for many frames; a shift under 4 px is ignored, and a shift
that would push the crop beyond `drift_px` is logged once and left alone.
The splits/counter rectangles measured at the lock move only by how far the
digits actually moved, not by the correction to the crop itself.

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

**The pane's own words.** A second, unrestricted sparse-text pass over the
same crop reads the letters: at every lock (with `[splits]` enabled) and
again every 60 s while locked, the bot reads the title row above the splits
(game and category, logged as `layout title: …` and recorded as a session
event) and every labelled reference time under the timer: Sum of Best, the
season best the streamer labels with a bare year (`2026: 11:35.1`), the
lifetime `PB:`, the `WR`. With `game.require_title_match = true` (off by
default) two consecutive reads naming a different game suspend recording and
one matching read resumes it, which is what a marathon broadcast needs, and
why a single garbled title cannot switch recording off for the rest of the
day. A reference value is written to `settings` (`ls_sob_ms`,
`ls_season_best_ms`, `ls_pb_ms`, `ls_wr_ms`) once seen twice, three times to
replace an established one, and only if it is plausible against
`detection.min_final_ms` and respects the order WR ≤ lifetime PB ≤ season
best, with a Sum of Best no slower than the season best (or PB) and no faster
than half of it. The season best then outranks `game.baseline_best` as the
record to beat (`<record_label> to beat is now … (from the layout)` in the
log; the site reads the same setting), and stored values are reloaded at
startup so a restart does not re-announce them. Only what the configured
rectangles span is decoded, so the `lifetime_sob` rectangle has to cover
those rows; when the pass cannot label any of them, that rectangle is still
read on its own as the Sum of Best.

**Splits, run numbers and golds.** LiveSplit shows the comparison time in
rows not yet reached and the actual time in completed ones, so a split is
detected by *change*: the column is read every `splits.read_every_secs` (5)
while a run is in progress (a column that has not changed pixel-wise is
replayed without OCR), each row's first value is its baseline, and a row
that later differs by more than `tolerance_ms` for `confirmations`
consecutive reads is that act's cumulative time, accepted only while the
timer itself was read within the last 5 s, not more than a few seconds past
the timer, after the previous act, and not below 90% of the previous act's
configured boundary (60% of its own for the first act). An act whose actual
time ties its comparison shows no change and is backfilled when a later act
proves it happened. The final act's split is never taken from the column,
where it could only be a misread of the comparison row; it is the finish
time, written when a run that already has splits finishes. The attempt
counter is read every 2 s until the run has a number, and only while the
timer was accepted within 3 s: LiveSplit bumps it the instant the runner
restarts, before the old run's reset is seen, so a dying run must not take
its successor's number. `counter.rs` decides which reading to believe: a
value far ahead of the last one for the time elapsed is refused, the first
value of a session needs three identical reads (two afterwards), a value
below half the last one means the streamer's counter restarted and numbering
follows it, and an adopted number is reverted (and cleared from the runs
that carry it) only when two runs in a row settle on lower values that
continue the sequence from before it. Golds are the fastest segment per act
that is at least 85% of that act's *median* segment (a misread column is far
further under), for the final act only from finished runs. `!golds` appends
their sum as Sum of Best once every act has one; the site shows it as "Sum of
best (tracked)" next to the runner's own Sum of Best row read off the layout
(`[lifetime_sob]`).

## Scripts and operations

| script | purpose |
|---|---|
| `scripts/build-release.sh` | build `target/release/ngtwitchtimer`, with the in-process OCR engine when the `~/.local/opt/ocr-dev` toolchain is staged (see Requirements), else the CLI engine |
| `scripts/run-live.sh` | supervise the live bot (restart on exit, log rotation, `logs/live.log`); run it inside tmux |
| `scripts/rollout.sh [--allow-dirty] [--smoke-vod <id>]` | ship a build to the live bot: refuses unless on `main` with a clean tree, builds (`build-release.sh`), runs the tests, smoke-replays two minutes of the most recent VOD session (from 40 minutes in) and refuses unless the timer read ≥80%, waits (up to an hour) for the bot to be between runs or offline, SIGTERMs it so the `run-live.sh` supervisor restarts it on the new binary (refuses if no supervisor is running), and confirms the new process came up |
| `scripts/replay-window.sh <cfg> <vod_id>\|<window.mp4> <start_secs> <dur_secs> [binary] [label]` | replay one window of a VOD and score the capture against the runner's own attempt counter (runs found, run numbers, lock events); run it with the old and the new binary before trusting any OCR or detection change. An existing `.mp4`/`.mkv` in place of the id replays a local recording (`source = "file"`); a file named `<label>-<vod>-<start>.mp4` is taken to begin at second `<start>` of its VOD, so the start is still given on the VOD's timeline, and its runs are dated from a placeholder epoch so the window count works |
| `scripts/obs-accuracy.sh <obs.jsonl>` | the label-free misread check over an observation log: consecutive running frames must advance by one frame interval within ±60 ms (`TOL_MS`); resets, frozen timers, values under 10 s, event frames and the frames after a lock are excluded and counted by reason. Prints the rate per pair and per frame, per reader, and the worst frames with the readings around them |
| `scripts/obs-diff.sh [-q] <a.jsonl> <b.jsonl>` | join two observation logs of the same window on the frame number and list the frames where OCR text, parsed value, phase, layout offset or events differ; summary line first, exit status like `diff`. Warns when the logs are not frame-aligned |
| `scripts/backfill-vods.sh <vod_id>...` | analyze Twitch VODs one after another straight from Twitch (no download), one database each in `backfill-db/vod-<id>.db` with its obs log in `backfill-logs/`; run several chains in parallel. It does not read `live.toml`: it writes its own config per VOD with the reference deployment baked in (channel, layouts, acts, 480p30, 2 fps, the glyph reader, `min_final_ms`, the AppImage `tessdata_path`), so edit the heredoc for another streamer. Workers run under `nice`; a rerun replaces an earlier pass over the same VOD |
| `scripts/import-vod.sh <vod_id> [--deploy] [--force]` | replace one broadcast day in the live database from its completed VOD database (refuses a VOD whose sessions are not all closed); one transaction, safe while the bot is running; normalises finished runs' final-act split to the finish time, renumbers attempts chronologically and runs `fill-run-numbers.sh`. Before replacing it compares the incoming day with the one in the live database (runs, numbered runs, session span) and refuses with exit 3 when the new pass holds under 90% of either count, so a pass that died partway cannot overwrite a fuller day; `--force` replaces anyway. `LIVE=copy.db` targets another database for a dry run |
| `scripts/import-when-done.sh <vod_id>...` | detached: import each VOD as its chain finishes and redeploy the site. A VOD the import gate refuses is left unmarked and reported; it is retried only when its database changes, and the final line names the refused ids (exit 3) |
| `scripts/list-vods.sh <channel> [--game <substring>]` | list a channel's archived VODs newest first (`id  date  hours  title`) from Twitch's GraphQL endpoint with `curl` + `jq`, so a backfill can be assembled without guessing ids; `--game` filters titles case-insensitively; falls back to `yt-dlp` (no dates) when GraphQL declines. `TWITCH_CLIENT_ID` overrides the web client-id |
| `scripts/rebackfill.sh [--chains N] [--label L] [--dry-run] <vod_id>...` | re-run VODs safely: archives their earlier passes to `backfill-db/rerun-<stamp>/` and `backfill-logs/rerun-<stamp>/`, strips the ids from `backfill-db/imported.txt`, then launches N detached `backfill-vods.sh` chains and one `import-when-done.sh` over the ids. Refuses ids a running chain or worker is already processing |
| `scripts/merge-backfill.sh [--swap]` | full chronological rebuild into `ninja-gaiden-merged.db` from every completed VOD database, plus live-tracked sessions on days no VOD covers (settings copied from the live db, run numbers filled); `--swap` moves it into place and keeps the old database as `ninja-gaiden-pre-merge-<timestamp>.db` (stop the bot before `--swap`) |
| `scripts/fill-run-numbers.sh [db]` | clear a LiveSplit run number that falls outside its two agreeing neighbours (a misread), then infer missing numbers where the arithmetic between known neighbours is unambiguous; safe to run while the bot is writing the database; reports `outliers_cleared`, `filled`, `coverage` |
| `scripts/build-site.sh [cfg]` / `deploy-site.sh [--infra]` / `deploy-if-live.sh` | `build-site.sh` bakes `report --json` into `site/index.html` under a lock (two builds overlap in normal operation), merging the WR and lifetime PB from speedrun.com when reachable (`curl` + `jq`; the game/category/user ids are hardcoded at its top; layout-read values win) and validating the JSON with `jq`. `deploy-site.sh` runs `fill-run-numbers.sh` on the live database, builds, uploads to S3 and invalidates CloudFront (`aws` CLI; `--infra` also deploys `infra/site-stack.yml`). `deploy-if-live.sh` is for a ten-minute cron and deploys only while an `hls` session is open |
| `scripts/backup-db.sh [db]` | nightly snapshot of the live database through sqlite's online `.backup` (safe while the bot writes), gzipped into `backups/`, 30 days kept; with `NG_BACKUP_S3=s3://bucket/prefix` also copied there (the public site bucket is refused) |
| `scripts/healthcheck.sh [-v]` | dead-man check for a ten-minute cron: the tmux supervisor exists, exactly one live bot process, no crash loop (at most 3 starts in 30 minutes), the observation log still grows while a live session is open, an offline poll within 35 minutes inside `active_hours`, over 5 GB of disk, the database readable. Each failing signal alerts once an hour and once more as an all-clear on recovery (state in `logs/health-state/`, record in `logs/health.log`); `-v` prints every signal. Always exits 0 |
| `scripts/daily-summary.sh [YYYY-MM-DD]` | one plain-text block for a day (default today): attempts, finishes and the best time with its LiveSplit run number, resets by act, coverage against his own counter span, sessions with capture health, glyph reader totals, VOD imports that landed and the day's healthcheck alerts; cron runs it at 23:58 |
| `scripts/notify.sh <subject>` (body on stdin) | delivery for the two monitoring scripts: `NG_ALERT_URL` posts ntfy-style (a URL containing `discord` gets a JSON `content` body), else `NG_ALERT_MAIL` goes through `mail`, else the message is appended to `logs/health.log`. Set the variables at the top of your own crontab, not in the tracked example |
| `scripts/crontab.example` / `install-cron.sh [--show]` | the reference deployment's schedule: a `@reboot` line that restarts the supervisor in tmux, the two site deploys, the backup, the ten-minute healthcheck and the daily summary. The installer replaces the grindlog lines in the user's crontab with the example's and leaves everything else alone |
| `scripts/make-test-video.sh` | synthetic timer video for end-to-end tests |
| `infra/site-stack.yml` | CloudFormation for the site (S3 + CloudFront + certificate + Route53 alias); pass your own `HostedZoneId` |

Typical rhythm: the live bot runs all day under `run-live.sh` in tmux from
the tracked `live.toml`; a change reaches it through `rollout.sh`; cron
(`scripts/crontab.example`, installed by `install-cron.sh`) runs
`deploy-if-live.sh` every ten minutes, `deploy-site.sh` and `backup-db.sh`
nightly, and restarts the supervisor after a reboot; old VODs are backfilled
in a few parallel `backfill-vods.sh` chains with `import-when-done.sh`
landing each day as it completes.

The scripts are written for the reference deployment and hardcode its names:
`live.toml`, `ninja-gaiden.db` and `obs-live.jsonl` in `run-live.sh`,
`rollout.sh`, `deploy-if-live.sh`, `deploy-site.sh` and `merge-backfill.sh`
(`import-vod.sh` takes `LIVE=` and `fill-run-numbers.sh` a `[db]` argument
to override), and `backfill-vods.sh` embeds its own config, channel and crop
rectangles included. Edit those for another deployment. Beyond the bot's own
ffmpeg and tesseract, the scripts need `bash` and `sqlite3`, the site
scripts `curl`, `jq` and `flock`, and `deploy-site.sh` the AWS CLI.

## Machine-readable data

The site publishes its data for bots and assistants as static JSON under
`https://ng.lexicone.com/api/v1/`, produced by `build-site.sh` from the same
report the page embeds and uploaded by `deploy-site.sh` on every deploy:

- `latest.json` (~4 KB): state of the grind in one document — today, the
  records with their scope and source, the last run and finish, streaks,
  golds, deaths by act. What a chat bot or an LLM tool call should fetch.
- `summary.json` (~35 KB): every aggregate the site shows, no per-run rows.
- `report.json` (~1 MB, 85 KB gzipped): every run, split and session.
- `index.json`: manifest with byte sizes and the build time; `/api/index.json`
  is the version root; `/llms.txt` is the plain-text entry point for
  assistants; `/api/v1/README.md` (tracked at `site/static/api/v1/README.md`)
  is the field reference.

`latest.json` is the projection `scripts/api-latest.jq` computes; the other
two are `jq` filters inline in `build-site.sh`. Everything is served with
`max-age=60` (the docs 3600), an ETag, and `Access-Control-Allow-Origin: *`
(a CloudFront response-headers policy in `infra/site-stack.yml`; a missing
key answers 404). Fields are only added within a version; a change of meaning
bumps the path. The feed names the game and category but not the streamer,
as the page does. Per-day files behind a manifest are the planned phase 2.

## Maintenance notes

- Twitch occasionally rotates the web player client-id or retires the GraphQL
  persisted query. Symptoms: GQL 400s in the log. Fix: see the comments at
  the top of `src/twitch_hls.rs` (one-line updates, current values are in
  streamlink's `twitch.py`).
- When the streamer starts a new season (he resets his splits file, and with
  it the comparison column and Sum of Best) nothing needs editing: the season
  best is read off the layout's own reference rows at each lock and every
  minute, stored in `settings` as `ls_season_best_ms` once seen twice (three
  times to replace an earlier value) and adopted as the record to beat for
  chat and the site, on restart too (`layout season best: …` then `PB to
  beat is now … (from the layout)` in the log). `game.baseline_best` only
  stands in until the layout has been read once; a theme whose rows cannot
  be read, or a `lifetime_sob` rectangle that does not span them, leaves the
  previous value in force. Delete the `ls_season_best_ms` row from
  `settings` to fall back to the config value.
- The binary re-execs itself once with `OMP_THREAD_LIMIT=1` when the variable
  is unset (libgomp reads it before `main`, so setting it later is too late):
  on these small crops one OpenMP thread is faster per call, and several
  workers sharing the cores no longer spin-wait each other to a crawl. A
  value you export yourself is honoured instead. The supervisor, backfill
  and replay scripts export it too.
- `NG_DUMP_PANE=1` makes the lock-time pane analysis save what it saw to
  `calibration/pane.png` (and each counter crop to `calibration/counter.png`)
  and log every word it read at debug level — the first thing to look at when
  a new layout reads its splits as blanks.
- `NG_DUMP_TIMER=1` saves the raw timer crop every 25th frame as
  `calibration/timer-<frame>.png` (in a `calibration/` directory beside the
  `obs_log`, or the working directory without one) and logs the threshold
  Otsu would pick against the configured one, for threshold tuning against
  real pixels. `NG_DUMP_TIMER=all` saves every frame: the glyph-reader
  training corpus described under *Reading the timer with templates*.
- `glyph reader declined N of the last M timer frames` in the log (checked
  per 600 frames, emitted when declines outnumber hits) means the templates
  do not cover what is on screen, a new theme or font; tesseract is reading
  those frames at roughly fifty times the cost. Retrain as described under
  *Reading the timer with templates*.
- Sessions record capture health; the site shows each day as `N of his M
  attempts captured` (M is the span of his own run counter) and flags a day
  (red dot on its chip, red "capture" word) under 85%, or, with no run
  numbers, when under 60% of frames read or fewer than half of 5+ runs carry
  a number. A weak day is the cue to replay that window with
  `scripts/replay-window.sh` and read its lock events; if the layout never
  locked at all, run `locate` on a frame from that VOD.

## Contributing

Issues and pull requests are welcome. The best first contributions are
layout configurations for other streamers/games (with a `locate` printout
and a frame), detection edge cases with an `obs_log` excerpt, and site
ideas. Please keep the bot itself Rust + ffmpeg + tesseract only; the
operations scripts may use common shell tools (`sqlite3`, `jq`, `curl`,
`flock`, the AWS CLI) but please do not add new interpreters. For any
change to OCR, locking or detection, include the `scripts/replay-window.sh`
lines for the old and the new binary on the same VOD window (see Scripts
and operations).

CI (`.github/workflows/ci.yml`) runs on every pull request and push to
`main`: `cargo fmt --all -- --check`, `cargo clippy --release --all-targets
-- -D warnings`, `cargo build --release`, `cargo test --release`, and
`bash -n` over `scripts/*.sh`. Run the same locally before opening a PR; a
formatting or clippy warning fails the check. The unit tests cover parsing,
the state machine, geometry and the splits tracker and need neither
tesseract nor ffmpeg. CI builds with default features, so the in-process OCR
(`--features leptess-ocr`, what `scripts/build-release.sh` uses) is not
compiled there.

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
