# latest.json: the one document a bot or an assistant fetches for "how is the
# grind going" — a projection of the report JSON the page embeds (see
# build-site.sh). Bounded in size; every time is in milliseconds with a
# formatted twin. Field reference: site/static/api/v1/README.md.
#
#   jq -c --arg tz "America/Los_Angeles" -f scripts/api-latest.jq report.json

def fmt: if . == null then null else
  ((. / 100 | round) as $t | ($t / 600 | floor) as $m | ($t - $m * 600) as $r
   | ($r / 10 | floor) as $s | ($r - $s * 10) as $d
   | if $m > 0 then "\($m):\(if $s < 10 then "0" else "" end)\($s).\($d)" else "\($s).\($d)" end) end;

(.generated_at_ms / 1000 | floor) as $gen
| ((.generated_at_ms + .day_offset_minutes * 60000) / 1000 | floor | strftime("%Y-%m-%d")) as $today
| (.runs | sort_by(.started_at_ms) | last) as $last
| (.finishes | last) as $lastfin
| (.pb_history | last) as $best
| (.daily | last) as $lastday
| {
  schema_version: 1,
  docs: "https://ng.lexicone.com/api/v1/README.md",
  generated_at_ms: .generated_at_ms,
  generated_at: ($gen | todate),
  stale_after_s: 900,
  timezone: $tz,
  day_offset_minutes: .day_offset_minutes,
  game: .current_game,
  category: .current_category,
  record_label: .record_label,
  attempt_numbering: {
    livesplit_attempt: "the streamer's own LiveSplit attempt counter, read off the layout; the run's identity, null when it was not read",
    tracked_ordinal: "grindlog's count of the attempts it saw for this game, in order; only used where no LiveSplit number exists"
  },
  live: {
    capturing: (any(.sessions[]; .ended_at_ms == null and .source == "hls")),
    session_started_at_ms: ([.sessions[] | select(.ended_at_ms == null and .source == "hls") | .started_at_ms] | first),
    note: "capturing means a live session was open when this file was built; this file is rebuilt every 10 minutes while live, nightly, and after each VOD import"
  },
  today: {
    day: $today,
    attempts: .today.attempts,
    finished: .today.finished,
    resets: .today.resets,
    best_ms: .today.best_ms,
    best: (.today.best_ms | fmt),
    note: "the streamer's local day at build time; after the nightly build this is the day that just closed"
  },
  records: {
    season_best_ms: .baseline_best_ms,
    season_best: (.baseline_best_ms | fmt),
    season_best_source: "the year-labelled row of the LiveSplit layout when it has been read, else the configured baseline",
    best_tracked_ms: ($best.time_ms),
    best_tracked: ($best.time_ms | fmt),
    best_tracked_at_ms: ($best.at_ms),
    best_tracked_livesplit_attempt: ($best.ls_attempt),
    sum_of_best_ms: .ls_sob_ms,
    sum_of_best: (.ls_sob_ms | fmt),
    references: [.references[] | {label, ms, time: (.ms | fmt)}]
  },
  last_run: (if $last == null then null else {
    started_at_ms: $last.started_at_ms,
    ended_at_ms: $last.ended_at_ms,
    outcome: $last.outcome,
    reset_reason: $last.reset_reason,
    final_time_ms: $last.final_time_ms,
    final_time: ($last.final_time_ms | fmt),
    last_timer_ms: $last.last_timer_ms,
    last_timer: ($last.last_timer_ms | fmt),
    livesplit_attempt: $last.ls_attempt,
    tracked_ordinal: $last.attempt_number
  } end),
  last_finish: (if $lastfin == null then null else {
    started_at_ms: $lastfin.started_at_ms,
    final_time_ms: $lastfin.final_time_ms,
    final_time: ($lastfin.final_time_ms | fmt),
    livesplit_attempt: $lastfin.ls_attempt,
    tracked_ordinal: $lastfin.attempt_number
  } end),
  streaks: .streaks,
  latest_day: (if $lastday == null then null else ($lastday + {
    best: ($lastday.best_ms | fmt),
    livesplit_attempts: (if $lastday.first_no != null and $lastday.last_no != null then $lastday.last_no - $lastday.first_no + 1 else null end)
  }) end),
  golds: [.golds[] | . + {gold: (.gold_ms | fmt)}],
  death_chart: .death_chart,
  survival: [.survival[] | {label, survived: .deaths, pct}],
  acts: .acts,
  links: {
    summary: "https://ng.lexicone.com/api/v1/summary.json",
    report: "https://ng.lexicone.com/api/v1/report.json",
    index: "https://ng.lexicone.com/api/v1/index.json",
    docs: "https://ng.lexicone.com/api/v1/README.md",
    site: "https://ng.lexicone.com/"
  }
}
