# Working on grindlog with Claude

Notes for an AI assistant (or a new contributor) working in this repository.
The README explains what the bot does and how to run it; this file is about
how to change it without breaking the deployment that depends on it.

## What this is

A Rust bot that watches a Twitch stream, reads the streamer's LiveSplit
timer off the video, logs every attempt to SQLite and publishes a records
site. One binary, `ngtwitchtimer`, with subcommands (`run`, `calibrate`,
`locate`, `report`, `glyphs`, `audit`). The reference deployment follows one
streamer around the clock and is the thing every change here ends up running
against.

Layout of `src/`:

- `app.rs` — the run loop: capture, layout probe/lock/re-anchor, timer read,
  splits and counter reads, state machine, database, chat, observation log.
- `glyph.rs` — the purpose-built timer reader (templates in
  `assets/glyphs.json`); `ocr.rs` — preprocessing and the tesseract engines.
- `state.rs` — the run state machine; `sanity.rs` — the smoothed clock;
  `counter.rs` — which attempt number to believe; `splits.rs` — per-act
  splits by change against the comparison column; `timeparse.rs` — timer
  text to milliseconds.
- `board.rs` — the pane read as a board (title, rows, time cells) and which
  game a title files under; `marathon.rs` — a board whose rows are ten
  different games, tracked by which of them have completed rather than by
  the timer (`[[games]] mode = "board"`); `roster.rs` — which of an event's
  ten games a row's OCR-read name is; `audit.rs` — the harness that scores a
  replayed marathon against the key its own board derives (`audit`).
- `capture.rs` / `twitch_hls.rs` — stream and VOD decoding via ffmpeg;
  `config.rs` — the TOML config (every field documented in
  `config.example.toml`); `db.rs`, `stats.rs`, `report.rs` — persistence and
  the site's JSON; `chat.rs` — Twitch chat.

## Ground rules

- **Rust only.** No Python in the toolchain or the scripts. Scripts may use
  bash, sqlite3, curl, jq, flock and the AWS CLI. Ad-hoc analysis in a
  scratch directory is fine; nothing committed depends on it.
- **Never push to `main`.** Branch, `gh pr create`, wait for CI, then
  `gh pr merge --squash --delete-branch`. Branch protection requires the CI
  check and linear history.
- **CI denies warnings.** Before pushing, run what CI runs:
  `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`,
  `cargo test --release`, and `for f in scripts/*.sh; do bash -n "$f"; done`.
- **`live.toml` is tracked** and read by the deployed binary, the site
  importer and the backfill workers. Config parsing rejects unknown fields,
  so a new field must ship in the binary first (`scripts/rollout.sh`) and be
  enabled in `live.toml` in a second PR and rollout. The reverse order
  crash-loops the live bot on its next restart.
- **Deploy with `scripts/rollout.sh`**, never by hand: it builds from a
  clean `main`, runs the tests and a two-minute smoke replay, waits for the
  live bot to be between runs, and restarts it under its supervisor. Check
  `logs/live.log` afterwards for the new session and its startup lines.
- **Tests carry the behaviour.** The state machine, parser, counter tracker,
  geometry and glyph reader are unit-tested (`tests/fixtures/glyph` holds
  real crops); `config.example.toml` and `live.toml` are parsed by a test,
  and every commented-out `# field = value` in the example is switched on
  and must validate. So in `config.example.toml` a comment line that starts
  with `# word = ` is read as a field: write prose comments so they don't
  look like one. Add a test with any behaviour change.

## How to test a change safely

Validate on recorded footage before the live stream ever sees it:

1. `scripts/replay-window.sh <config> <vod_id> <start_secs> <dur_secs>`
   replays a window of a Twitch VOD with a given binary and config and
   prints capture metrics (frames parsed, runs, run numbers against the
   streamer's own counter span, lock events). Compare a candidate build
   against the current one on the same windows; pick windows that contain
   the thing you changed (scene switches for probing, the NES-styled theme
   for legibility, a stretch with no timer for cost). A local recording
   works in place of the VOD id: the reference box keeps pinned windows
   under `vods/windows/<label>-<vod>-<start>.mp4` (ignored by git), which
   replay identically to the stream and survive Twitch's VOD expiry.
2. `[debug] obs_log` writes one JSON line per frame (OCR text, parsed value,
   smoothed clock, phase, events, layout offset, which reader read it).
   `scripts/obs-diff.sh <a.jsonl> <b.jsonl>` joins two logs of the same
   window on the frame number and lists where OCR text, parsed value,
   phase, offset or events differ; that diff explains most regressions.
3. `scripts/obs-accuracy.sh <obs.jsonl>` is the label-free accuracy check
   that needs no ground truth: between consecutive frames of a running
   timer the reading must advance by the frame interval; a jump beyond
   ±60 ms is a misread. It excludes resets, the frozen timer after a
   finish, values under 10 s and the frames after a lock, reports the rate
   per reader and prints the worst frames with their neighbours.
4. Only then `scripts/rollout.sh`.

A marathon day is a different scene and a different tracker, so it has its
own replay: `scripts/replay-arcathlon.sh <vod_id>` writes the whole
broadcast into `arcathlon-db/vod-<id>.db` with its board log beside it, and
every completed row of the board lands as a run of its own game under
category `Arcathlon`. It bakes its own config (the marathon total as the
timer, the pane crop raised to take in the title row, `[[games]] mode =
"board"`), so it needs no live config and cannot touch the live database.
Start it at second 0: a row that already carries its time when the board
first comes into view was finished before the bot looked and is not
recorded. `debug.board_log` is what to read when a game is missing — the
board is cumulative, so an unreadable stretch delays a reading, it does not
destroy it.

**And a marathon board is its own answer key**, which is what makes a change
to it checkable at all. A game he has finished shows his result and one he
has not reached shows the comparison; in a single frame they are the same
kind of number, but over a whole broadcast the row he plays CHANGES exactly
once and no other row changes at all. So a board log says which of the
event's ten games he played and in what time, with no video, no timer, no
title and no key read off the stream by hand.
`scripts/audit-arcathlon.sh` (the `audit` subcommand, `src/audit.rs`) replays
every broadcast under `arcathlon-db/` through the tracker's own decision
sequence and prints, per broadcast, the roster its rows identify, every row
with the first and last value it settled on, and the differences both ways:
a game played and not recorded, a run recorded the board does not account
for, a run filed under a game the board gave to another row, and two runs of
one broadcast under one name — which an event of ten distinct games can
never have, so that last one is a defect on the board's own evidence with no
key having to be right. Run it before and after any change to
`src/marathon.rs`, `src/roster.rs`, `src/signature.rs` or the gate in
`src/sanity.rs`, and diff the `REC`, `BAD` and `SUM` lines; it takes about a
second over all 38 captures. The same check runs as a test:

    ARCATHLON_DB=arcathlon-db cargo test --release \
      replays_every_captured -- --ignored --nocapture

Two honest limits. Its key is OCR like everything it checks, and it
disagrees with the keys read off the video by hand in one known place
(2827296024's Astyanax, where the hand-read key is right) — so read the row
off the board log before believing either. And it folds names onto games
with the same `src/roster.rs` the tracker uses, so it cannot score that
module: a roster that named the wrong game would name it wrong for both.
What it scores is the plumbing — which rows were played, which were
recorded, at what time, under which of the event's games — and `roster.rs`'s
own tests hold the other end.

The unit tests run without ffmpeg or tesseract. Anything that needs video
needs `ffmpeg`; anything that reads splits, the counter or a fallback timer
frame needs `tesseract` (see the README's Requirements for the user-space
install and the in-process build).

## Operating facts

- The live bot runs supervised (`scripts/run-live.sh`, in a tmux session on
  the reference box) and logs to `logs/live.log`; observations go to
  `obs-live.jsonl`. `kill` (SIGTERM) is a clean stop; the supervisor
  restarts it. The schedule lives in `scripts/crontab.example` (a reboot
  line that restarts the supervisor, the site deploys, the nightly
  `backup-db.sh`, the ten-minute `healthcheck.sh` and the 23:58
  `daily-summary.sh`); `scripts/install-cron.sh` installs it idempotently.
  Backups land in `backups/` (ignored by git), 30 days kept.
- Monitoring writes to `logs/health.log` and `logs/summary.log`; it only
  leaves the box when `NG_ALERT_URL` (ntfy topic or Discord webhook) or
  `NG_ALERT_MAIL` is set at the top of the owner's crontab. Those values are
  secrets and never go in the tracked example. `scripts/healthcheck.sh -v`
  shows every signal's state on demand.
- tesseract must run with `OMP_THREAD_LIMIT=1`; the binary re-execs itself
  with it set, and the scripts export it. Several tesseract workers on one
  box otherwise spin-wait each other to a crawl.
- The backfill (`scripts/backfill-vods.sh`) analyses whole VODs into
  per-VOD databases under `backfill-db/`; `scripts/import-when-done.sh`
  imports each finished one into the live database and redeploys the site.
  It treats any per-VOD database with a closed session as finished, so
  re-run VODs through `scripts/rebackfill.sh`, which archives the old
  passes and strips the ids from `backfill-db/imported.txt` before it
  launches the chains; by hand, do those two steps first. `import-vod.sh`
  refuses to replace a day with a thinner pass (under 90% of the runs or
  numbered runs); `--force` is for days whose earlier rows were phantom
  fragments. `scripts/list-vods.sh <channel> --game "ninja gaiden"` gives
  the ids with dates.
- The site (`site/template.html` + `report --json`) is one self-contained
  page; `scripts/build-site.sh` builds it and `scripts/deploy-site.sh`
  uploads it. A single uncaught JavaScript error blanks the whole page, so
  check it in a browser after touching the template.
- The same build writes the machine-readable feed (`site/api/v1/`, see the
  README's *Machine-readable data*): `latest.json` comes from
  `scripts/api-latest.jq`, `summary`/`report`/`index` from `jq` in
  `build-site.sh`, and the per-day feed (`days/<day>.json`, `history.json`,
  `schema.json`, `manifest.json`) from `report --api-dir` in `src/api.rs`,
  whose structs are the contract: the schema is generated from them, and a
  closed day's bytes must not change between builds unless its rows did
  (no timestamps in day files, sorted rows; the tests check it). Within
  `/api/v1/` only add fields; a change of meaning is a new version path. The
  feed does not name the streamer (the page does not either); that is the
  owner's call, not a default to flip.
- The board reader (`src/board.rs`) runs on the pane passes but only speaks
  with `game.follow_title = "log"`: `layout snapshot:` lines and `layout`
  session events, one per distinct board, naming the `[[games]]` alias or
  the title it would file the board under. Shadow mode: it changes nothing
  that is recorded. Its fixtures (`tests/fixtures/board/`) are real panes
  with ground truth; a change to the reader must keep them passing.
  `build-site.sh` strips only the `title` events from the page's copy of
  the report; `layout` events stay (a handful per session).
- `pkill -f`/`pgrep -f` patterns must not appear literally in the same
  command line (`ngtwitchtimer --config live.toml ru[n]`), or they match the
  shell running them.

## The glyph reader

The timer is read by templates harvested from the streamer's own footage
(`assets/glyphs.json`), with tesseract for the frames it declines. The
session-close line in the log reports `glyph reader N read / M declined`;
a window of mostly declines logs a warning that the templates do not cover
what is on screen (a new theme or font). Retraining is documented in the
README under *Reading the timer with templates*; the corpus must come from
a replay with `reader = "tesseract"`, because templates are labelled by
tesseract's readings and frames the glyph reader read are skipped.

## Things that looked like bugs and were not

- tesseract reads the small hundredths pair systematically wrong on some
  themes ("11" as "14", "77" as "71"). A disagreement between the glyph
  reader and tesseract is not evidence against the glyph reader; look at
  the pixels.
- The layout's crop rests anywhere within a few pixels of its anchor; where
  it settles is path-dependent and harmless for the glyph reader.
- The health line's "layout events" count includes the once-a-minute title
  reads, not just re-anchors.
- The attempt counter is static text: a misread repeats identically frame
  after frame, so "seen N times" proves nothing. `counter.rs` is what
  decides.
