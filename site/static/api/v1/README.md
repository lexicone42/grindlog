# Grind Log data, v1

Static JSON published beside the records site at https://ng.lexicone.com/.
Every file is a projection of the same report the page embeds, rebuilt every
10 minutes while a stream is live, nightly at 23:50 in the streamer's time
zone, and after each VOD import. Served with an ETag and
`Access-Control-Allow-Origin: *`: poll with `If-None-Match` and a 304 costs
nothing. `Cache-Control` is `max-age=60` for the JSON files; 3600 for this
document, `schema.json`, `/llms.txt` and `/api/index.json`; and for a
*closed* day file of the per-day feed `max-age=60, s-maxage=31536000` — a
minute in your client, a year at the edge, which is invalidated when such a
file changes.

All times are integers in **milliseconds**; fields ending in `_ms` are
durations or epoch timestamps (UTC), and most carry a formatted twin without
the suffix (`"11:35.1"`). Days (`"2026-09-04"`) are the streamer's local day
(`timezone`, an IANA name; `day_offset_minutes` is the offset in force at
build time).

## Files

| file | what | size |
|---|---|---|
| `/api/v1/latest.json` | state of the grind in one small document | ~3 KB |
| `/api/v1/summary.json` | every aggregate the site shows, no per-run rows | ~35 KB |
| `/api/v1/report.json` | the whole dataset: runs, splits, sessions | ~1 MB (85 KB gzipped) |
| `/api/v1/index.json` | manifest: files, byte sizes, build time | <1 KB |
| `/api/v1/manifest.json` | the per-day feed's index: every day file with size and sha256, the records, the last transition | ~7 KB |
| `/api/v1/days/<YYYY-MM-DD>.json` | one broadcast day: its runs with splits, its sessions with capture health | 15–80 KB |
| `/api/v1/history.json` | per-day stats and every finish, no runs | ~30 KB |
| `/api/v1/schema.json` | JSON Schema of the manifest, a day file and history | ~18 KB |
| `/api/index.json` | version root: which versions exist | <1 KB |
| `/llms.txt` | plain-text entry point for assistants | ~2 KB |

The first four carry an envelope: `schema_version` (1), `generated_at_ms`,
`generated_at` (ISO 8601, UTC), `timezone`, `docs` (this document). The
per-day feed puts its build time in the manifest only (see below).
Fields are only ever added within a version; a removal or a change of
meaning bumps the version and the path.

## Two attempt numbers

- `livesplit_attempt` (`ls_attempt` in the report): the runner's own LiveSplit
  attempt counter, read off the layout while the run was in progress. It is
  the run's identity and what the site shows. `null` when it was not read.
- `tracked_ordinal` (`attempt_number` in the report): the bot's count of the
  attempts it saw for this game, chronological. Only meaningful where no
  LiveSplit number exists.

Per day, `last_no - first_no + 1` over the LiveSplit numbers is the number of
attempts the runner really made; `attempts` is how many the bot captured.

## `latest.json`

- `game`, `category`, `record_label` — what is tracked and what the tracked
  best is called (`"season best"`: the runner's comparison resets each season
  of his splits file).
- `live.capturing` — a live capture session was open when the file was
  built; `live.session_started_at_ms`. This is a cron-era signal, not a
  heartbeat: see `stale_after_s` (900) against `generated_at_ms`.
- `today` — `day`, `attempts`, `finished`, `resets`, `best_ms`/`best`. The
  streamer's local day at build time; after the nightly build it is the day
  that just closed until the next live build.
- `records` — `season_best_ms` (the year-labelled row of the layout when it
  has been read, else the configured baseline; `season_best_source` says
  which in words), `best_tracked_ms` (the fastest finish the bot has
  recorded, with `best_tracked_at_ms` and `best_tracked_livesplit_attempt`),
  `sum_of_best_ms` (the layout's own Sum of Best row), `references`
  (`[{label, ms, time}]`: the world record and the lifetime PB).
- `last_run` — the most recent attempt by start time: `outcome`
  (`"finished"` or `"reset"`), `reset_reason` (`null`, `"zeroed"`,
  `"tooshort"`, `"desync"`, `"disappeared"`), `final_time_ms` (finishes),
  `last_timer_ms` (where a reset died), both numbers.
- `last_finish` — the most recent finished run.
- `streaks` — `attempts`, `finished`, `attempts_per_finish`,
  `current_reset_streak` (resets since the last finish),
  `longest_reset_streak`.
- `latest_day` — the last broadcast day's row from `daily` plus
  `livesplit_attempts` (his real count that day).
- `golds` — `[{act_index, act_name, gold_ms, gold, samples, set_at_ms}]`:
  the fastest tracked segment per act (segments far under the act's usual
  time are treated as misreads and skipped; the final act's gold comes only
  from finishes).
- `death_chart` — `[{label, deaths, pct}]`: resets per act, `pct` of resets.
- `survival` — `[{label, survived, pct}]`: attempts that got past each act,
  `pct` of attempts.
- `acts` — `[{name, end_ms}]`: the configured act boundaries (cumulative);
  the last act has `end_ms: null`.
- `links` — the other files and the site.

## `summary.json`

The report without `runs`, `splits_by_run` and `recent_runs`, and with
sessions' diagnostic `events` removed. Keys: `current_game`,
`current_category`, `record_label`, `references`, `baseline_best_ms` (=
season best), `ls_pb_ms`, `ls_sob_ms`, `day_offset_minutes`, `acts`,
`summaries` (per game/category totals), `today`, `daily`
(`[{day, attempts, finished, resets, best_ms, first_no, last_no}]`, oldest
first), `death_chart`, `survival` (note: here the count field is named
`deaths` for historical reasons and means *survived*), `pb_history`
(`[{at_ms, attempt_number, ls_attempt, time_ms}]`, every improvement of the
tracked best, chronological), `streaks`, `golds`, `finishes`
(`[{attempt_number, ls_attempt, started_at_ms, final_time_ms}]`,
chronological), `sessions` (`[{id, started_at_ms, ended_at_ms, source,
tag, attempts, finished, best_ms, frames, parsed, probing, relocks,
counter_reads}]`, newest first; `source` is `"hls"` for live capture and
`"vod"` for a VOD re-analysis; `frames`/`parsed` are capture health).

## `report.json`

Everything in `summary.json` plus:

- `runs` — every attempt: `id`, `game`, `category`, `attempt_number`,
  `ls_attempt`, `session_id`, `started_at_ms`, `ended_at_ms`, `outcome`,
  `reset_reason`, `final_time_ms`, `last_timer_ms`. Not sorted by time:
  order by `started_at_ms`. `started_at_ms` is unique and survives
  re-imports; `id` does not.
- `splits_by_run` — `{run_id: [{act_index, cumulative_ms}]}` for runs whose
  splits were read; the final act's split is the finish time.
- `recent_runs` — the last 15 runs by id, with the same fields as `runs`.
- `sessions[*].events` — diagnostic layout events (locks, re-anchors, the
  once-a-minute title reads).

## The per-day feed: `manifest.json` and `days/`

For a reader that keeps its own copy and wants to fetch only what changed.
Start at `/api/v1/manifest.json` and follow `days[]`; the whole history is
one file per broadcast day, and a day that is over changes rarely.

**How to poll.** Fetch the manifest every `stale_after_s` seconds (900): it
is rebuilt every ten minutes while a stream is live, nightly, and after each
VOD import, so polling faster gains nothing, and a manifest older than
`stale_after_s` means no live build has happened — the stream is off or the
bot is down. For each entry in `days[]`, compare `sha256` with the copy you
hold and fetch `path` (relative to the manifest) only when it differs. A
`closed` day is cached a year at the edge and a minute in your client; its
bytes change rarely, but they do change, whenever the database rows behind
it are edited: a VOD import of that day — which also renumbers
`attempt_number` across the *whole* database, so every later day changes
with it — run numbers filled in afterwards, a corrected time. Do not assume
a closed file is final: the manifest's `sha256` is the only truth about it,
and a day whose `sha256` changed should be resynced whole. The day named by
`today` is the live one when it has a file (an idle day has none); it and
any day with a session still open are never closed. `history.json` and
`schema.json` are listed under `files` the same way.

**`manifest.json`** — `schema_version`, `generated_at_ms`/`generated_at`,
`stale_after_s`, `today` (the streamer's local day at build time),
`day_offset_minutes`, `game`, `category`, `record_label`,
`attempt_numbering` (the two attempt numbers explained in words, see below),
`records`, `last_transition`, `days`, `files`.

- `records` — `[{label, ms, scope, source}]`, the times to beat. `scope` is
  `"season"` (the runner's comparison, which resets with each season of his
  splits file — the `record_label` entry) or `"lifetime"` (his lifetime PB,
  the world record). `source` is `"layout"` when the value was read off the
  LiveSplit layout's own reference rows, which he keeps current, and
  `"config"` when it is the deployment's configured value standing in until
  the layout has been read. Only what the bot itself holds: the speedrun.com
  values the site build merges into `report.json`'s `references` when the
  config has no WR or lifetime PB of its own are *not* in the manifest.
- `last_transition` — the bot's most recent state-machine transition,
  `{at_ms, from, to, game, category, detail}` (phases such as `IDLE`,
  `RUNNING`, `FINISHED`; `detail` is free text like `final_ms=696810`), or
  `null` before the first.
- `days` — `[{day, path, closed, bytes, sha256}]`, sorted by day. `sha256`
  is the lowercase hex SHA-256 of the file's exact bytes.
- `files` — `{history: {path, bytes, sha256}, schema: {…}}`.

**`days/<YYYY-MM-DD>.json`** — `day`, `closed`, `stats`, `sessions`, `runs`.
No timestamp inside: the file is a pure function of the rows.

- `stats` — `attempts`, `finished`, `resets`, `best_ms`, `first_no`,
  `last_no`: the day's row of `daily`, computed from the runs below.
- `sessions` — the sessions that started on this day or recorded a run on
  it, oldest first, so every non-null `session_id` resolves within the file:
  `id`, `started_at_ms`, `ended_at_ms` (`null` while ongoing), `source`,
  `tag`, `attempts`/`finished`/`best_ms` (over the whole session), capture
  health `frames`, `parsed`, `probing`, `relocks`, `counter_reads`, and
  `events` (`[{t, k, d}]`, diagnostic). `vod_id` and `vod_created_at_ms`
  appear only where the deployment publishes VOD links and the VOD is known;
  a run sits at `started_at_ms - vod_created_at_ms` into it.
- `runs` — every attempt that started on this day, oldest first: `id`,
  `attempt_number`, `ls_attempt`, `started_at_ms`, `ended_at_ms`, `outcome`,
  `reset_reason`, `final_time_ms`, `last_timer_ms`, `session_id`, and
  `splits` (`[{act_index, act_name, cumulative_ms}]`, in act order; the final
  act's split is the finish). **`id` is the run's `started_at_ms`**, or
  `started_at_ms + n` (n = 1, 2, …) for the later, by database id, of two
  runs that share a start — unique within the feed, always an integer.
  Database ids are not published. The id is stable across re-imports of the
  same VOD (the start time is what the importer preserves), but when a
  live-captured day is replaced by its VOD pass the day is re-keyed: key your
  copy on (`day`, `id`) and resync a day whose `sha256` changed rather than
  merging run by run. `session_id` is a database id and can change with a
  re-import; it is `null` when the run has none or its session row is gone,
  so a non-null value always resolves in this file's `sessions`.

**Attempt numbering, again.** `ls_attempt` is the runner's own LiveSplit
attempt counter, read off the layout while the run was in progress; it is
the number he and the site use, and `null` when it was not read.
`attempt_number` is the bot's own count of the runs it saw, renumbered
chronologically by imports; use it only to name a run that has no
`ls_attempt`. The manifest's `attempt_numbering` says the same in words.

**`history.json`** — `days` (`[{day, attempts, finished, resets, best_ms,
first_no, last_no}]`, one per day with captured runs, oldest first) and
`finishes` (`[{attempt_number, ls_attempt, started_at_ms, final_time_ms}]`,
oldest first): what a chart needs without downloading the runs.

**`schema.json`** — a JSON Schema (draft 2020-12) generated from the code
that writes these files. Its `properties.manifest`, `properties.day` and
`properties.history` are the three documents; shared types are under
`$defs`. Every field above carries a description there.

## Provenance

Every value was read off the public video by OCR. The timer is read by a
template matcher trained on this layout's font and is accurate to the
hundredth on well over 99% of frames; a finish is the timer frozen for
several frames, so finish times are exact to what the display showed. The
attempt counter, the splits column and the layout's title and reference rows
are read by tesseract with plausibility checks; a small share of runs has no
LiveSplit number (`null`), and a split may be missing where the runner's
segment tied his comparison to the tenth.
