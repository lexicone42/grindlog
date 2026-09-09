# Operations

Running the deployed bot: the scripts, the schedule, and the things that
have bitten before.


| script | purpose |
|---|---|
| `scripts/build-release.sh` | build `target/release/ngtwitchtimer`, with the in-process OCR engine when the `~/.local/opt/ocr-dev` toolchain is staged (see Requirements), else the CLI engine |
| `scripts/run-live.sh` | supervise the live bot (restart on exit, log rotation, `logs/live.log`); run it inside tmux |
| `scripts/rollout.sh [--allow-dirty] [--smoke-vod <id>]` | ship a build to the live bot: refuses unless on `main` with a clean tree, builds (`build-release.sh`), runs the tests, smoke-replays two minutes of the most recent VOD session (from 40 minutes in) and refuses unless the timer read ≥80%, waits (up to an hour) for the bot to be between runs or offline, SIGTERMs it so the `run-live.sh` supervisor restarts it on the new binary (refuses if no supervisor is running), and confirms the new process came up |
| `scripts/replay-window.sh <cfg> <vod_id>\|<window.mp4> <start_secs> <dur_secs> [binary] [label]` | replay one window of a VOD and score the capture against the runner's own attempt counter (runs found, run numbers, lock events); run it with the old and the new binary before trusting any OCR or detection change. An existing `.mp4`/`.mkv` in place of the id replays a local recording (`source = "file"`); a file named `<label>-<vod>-<start>.mp4` is taken to begin at second `<start>` of its VOD, so the start is still given on the VOD's timeline, and its runs are dated from a placeholder epoch so the window count works |
| `scripts/obs-accuracy.sh <obs.jsonl>` | the label-free misread check over an observation log: consecutive running frames must advance by one frame interval within ±60 ms (`TOL_MS`); resets, frozen timers, values under 10 s, event frames and the frames after a lock are excluded and counted by reason. Prints the rate per pair and per frame, per reader, and the worst frames with the readings around them |
| `scripts/obs-diff.sh [-q] <a.jsonl> <b.jsonl>` | join two observation logs of the same window on the frame number and list the frames where OCR text, parsed value, phase, layout offset or events differ; summary line first, exit status like `diff`. Warns when the logs are not frame-aligned |
| `scripts/backfill-vods.sh <vod_id>...` | analyze Twitch VODs one after another straight from Twitch (no download), one database each in `backfill-db/vod-<id>.db` with its obs log in `backfill-logs/`; run several chains in parallel. It does not read `live.toml`: it writes its own config per VOD with the reference deployment baked in (channel, layouts, acts, 480p30, 2 fps, the glyph reader, `min_final_ms`, the AppImage `tessdata_path`), so edit the heredoc for another streamer. Workers run under `nice`; a rerun replaces an earlier pass over the same VOD |
| `scripts/replay-arcathlon.sh <vod_id>...` | replay marathon broadcasts, one database each in `arcathlon-db/vod-<id>.db` beside its board log, obs log and bot log; run several chains in parallel. It writes its own config per VOD — the marathon total as the timer (its own crop and threshold, the offset search off, tesseract rather than the glyph templates), the pane crop raised to include the title row, and the `[[games]]` entry in `mode = "board"` — so every completed row lands as a run of its own game under category `Arcathlon`, and the base `[game]` is title-gated so the timer records nothing. `ARCA_OUT`, `ARCA_FPS`, `ARCA_BIN`, `ARCA_START` and `ARCA_NICE` override the output directory, frame rate, binary, starting second and worker priority; a rerun replaces an earlier pass. Start it at second 0: a row already carrying its time when the board first comes into view is not recorded |
| `scripts/audit-arcathlon.sh` | check every replayed marathon broadcast against the answer key its own board derives — over a whole broadcast the row he plays changes exactly once and no other row changes at all, so the board says which of the event's ten games he played and in what time, with no video, no timer, no title and no key read off the stream by hand. Replays each board log through the tracker's own decision sequence (`ngtwitchtimer audit`, `src/audit.rs`) and prints per broadcast: the roster its rows identify, every row with the first and last value it settled on, and the differences in both directions — a game played and not recorded, a run recorded the board does not account for, a run filed under a game the board gave to another row, and two runs of one broadcast under one name (which an event of ten distinct games can never have). Run it before and after a change and diff the `REC`, `BAD` and `SUM` lines; also runs as `ARCATHLON_DB=arcathlon-db cargo test --release replays_every_captured -- --ignored --nocapture`. `ARCA_OUT`, `ARCA_BIN`, `ARCA_ROSTER` override the capture directory, binary and roster file. The key is OCR like the thing it checks and disagrees with the hand-read keys in one known place (2827296024's Astyanax, where the hand-read key is right) |
| `scripts/import-vod.sh <vod_id> [--deploy] [--force]` | replace one broadcast day in the live database from its completed VOD database (refuses a VOD whose sessions are not all closed); one transaction, safe while the bot is running, held under the site build's lock (`.build-site.lock`, up to 120 s) so it cannot commit in the middle of a feed build; normalises finished runs' final-act split to the finish time, renumbers attempts chronologically and runs `fill-run-numbers.sh`. Before replacing it compares the incoming day with the one in the live database (runs, numbered runs, session span) and refuses with exit 3 when the new pass holds under 90% of either count, so a pass that died partway cannot overwrite a fuller day; `--force` replaces anyway. `LIVE=copy.db` targets another database for a dry run |
| `scripts/import-arcathlon.sh <vod_id>... [--deploy]` | import a marathon broadcast's games from its `arcathlon-db/vod-<id>.db` into the live database. Unlike `import-vod.sh` this is **additive and scoped to the VOD**, never to the day: he has run Ninja Gaiden in the morning and a marathon the same afternoon (2026-07-23, two VODs), so replacing the day would delete the morning's runs. It removes only what an earlier import of this same VOD left — matched on the session's `vod_id` — then inserts that VOD's session and one run per completed board row, each under the game the board named and category `Arcathlon`. One transaction per VOD, safe while the bot is running, idempotent on a re-run. Afterwards it re-counts the day's runs of other categories and stops if any went missing. `LIVE=copy.db` rehearses against a copy |
| `scripts/import-when-done.sh <vod_id>...` | detached: import each VOD as its chain finishes and redeploy the site. A VOD the import gate refuses is left unmarked and reported; it is retried only when its database changes, and the final line names the refused ids (exit 3) |
| `scripts/list-vods.sh <channel> [--game <substring>]` | list a channel's archived VODs newest first (`id  date  hours  title`) from Twitch's GraphQL endpoint with `curl` + `jq`, so a backfill can be assembled without guessing ids; `--game` filters titles case-insensitively; falls back to `yt-dlp` (no dates) when GraphQL declines. `TWITCH_CLIENT_ID` overrides the web client-id |
| `scripts/rebackfill.sh [--chains N] [--label L] [--dry-run] <vod_id>...` | re-run VODs safely: archives their earlier passes to `backfill-db/rerun-<stamp>/` and `backfill-logs/rerun-<stamp>/`, strips the ids from `backfill-db/imported.txt`, then launches N detached `backfill-vods.sh` chains and one `import-when-done.sh` over the ids. Refuses ids a running chain or worker is already processing |
| `scripts/merge-backfill.sh [--swap]` | full chronological rebuild into `ninja-gaiden-merged.db` from every completed VOD database, plus live-tracked sessions on days no VOD covers (settings copied from the live db, run numbers filled); `--swap` moves it into place and keeps the old database as `ninja-gaiden-pre-merge-<timestamp>.db` (stop the bot before `--swap`) |
| `scripts/fill-run-numbers.sh [db]` | clear a LiveSplit run number that falls outside its two agreeing neighbours (a misread), then infer missing numbers where the arithmetic between known neighbours is unambiguous; safe to run while the bot is writing the database; reports `outliers_cleared`, `filled`, `coverage` |
| `scripts/build-site.sh [cfg]` / `deploy-site.sh [--infra]` / `deploy-if-live.sh` | `build-site.sh` bakes `report --json` into `site/index.html` under a lock (two builds overlap in normal operation), merging the WR and lifetime PB from speedrun.com when reachable (`curl` + `jq`; the game/category/user ids are hardcoded at its top; layout-read values win) and validating the JSON with `jq`. `deploy-site.sh` runs `fill-run-numbers.sh` on the live database, builds, uploads to S3 and invalidates CloudFront (`aws` CLI; `--infra` also deploys `infra/site-stack.yml`). `deploy-if-live.sh` is for a ten-minute cron and deploys only while an `hls` session is open |
| `scripts/backup-db.sh [db]` | nightly snapshot of the live database through sqlite's online `.backup` (safe while the bot writes), gzipped into `backups/`, 30 days kept; with `NG_BACKUP_S3=s3://bucket/prefix` also copied there (the public site bucket is refused) |
| `scripts/healthcheck.sh [-v]` | dead-man check for a ten-minute cron: the tmux supervisor exists, exactly one live bot process, no crash loop (at most 3 starts in 30 minutes), the observation log still grows while a live session is open, an offline poll within 35 minutes inside `active_hours`, over 5 GB of disk, the database readable, and the last deploy (`logs/deploy.log`, entries start with `=== <time> deploy start`) reached its `live:` line and printed no `!!!` line (the per-day feed not built) when it started within the last 20 minutes. Each failing signal alerts once an hour and once more as an all-clear on recovery (state in `logs/health-state/`, record in `logs/health.log`); `-v` prints every signal. Always exits 0 |
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
  training corpus described under *Reading the timer with templates* on this page.
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

