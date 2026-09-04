# Grind Log data, v1

Static JSON published beside the records site at https://ng.lexicone.com/.
Every file is a projection of the same report the page embeds, rebuilt every
10 minutes while a stream is live, nightly at 23:50 in the streamer's time
zone, and after each VOD import. Served with `Cache-Control: max-age=60`
(3600 for this document), `Access-Control-Allow-Origin: *`, and an ETag:
poll with `If-None-Match` and a 304 costs nothing.

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
| `/api/index.json` | version root: which versions exist | <1 KB |
| `/llms.txt` | plain-text entry point for assistants | ~2 KB |

Every file carries an envelope: `schema_version` (1), `generated_at_ms`,
`generated_at` (ISO 8601, UTC), `timezone`, `docs` (this document).
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

## Provenance

Every value was read off the public video by OCR. The timer is read by a
template matcher trained on this layout's font and is accurate to the
hundredth on well over 99% of frames; a finish is the timer frozen for
several frames, so finish times are exact to what the display showed. The
attempt counter, the splits column and the layout's title and reference rows
are read by tesseract with plausibility checks; a small share of runs has no
LiveSplit number (`null`), and a split may be missing where the runner's
segment tied his comparison to the tenth.
