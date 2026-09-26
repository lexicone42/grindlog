# Big 20 practice days

The streamer is preparing for **the Big 20** — twenty NES games raced back
to back, somebody else's event with a published list
([race #23](https://thebig20nes.com/race-23/), 2026-10-10). Preparation is
weeks of grinding those twenty one at a time.

It is not a marathon day and shares nothing with one. A marathon puts ten
games on one board and is tracked by which rows have completed
([marathons.md](marathons.md)); a practice day is his **ordinary LiveSplit
pane** — one game, six-ish rows, a timer that resets — pointed at a
different game. So it needs no special scene, no board mode and no separate
layout: it needs the live config, and `follow_title = "track"`.

## What records them

`game.follow_title = "track"` (see [detection.md](detection.md), *Recording
the other game*). A board the identity gate convicts **and** whose header
names a game on a shipped roster has its runs recorded, under that game and
the category the roster gives — `assets/big20-roster.toml` says
`category = "Big 20 #23"`. A board that convicts without naming still
records nothing.

The gate is active whatever `follow_title` says. So a pass over a practice
day WITHOUT `track` records nothing on it rather than recording it wrong,
which is why `scripts/backfill-vods.sh` — the Ninja Gaiden backfill, which
does not set it — is the wrong tool for these days and cannot damage them
either.

## Backfilling a past practice day

Two scripts, in the shape the marathon pair established and for the same
reasons.

`scripts/replay-big20.sh <vod_id>...` replays a whole broadcast into
`big20-db/vod-<id>.db`. Unlike the marathon replay it does **not** bake its
own config: it takes `live.toml` and overrides only the stream, the database
and the observation log, because a practice board is his normal pane and a
second copy of those layouts here would drift from the deployment silently.
It refuses to run if the config it was handed does not set
`follow_title = "track"`, and refuses if the database path still points at
the live database after the substitution.

`scripts/after-broadcast.sh --rollout` does the evening's sequence unattended: it
waits for the live session to close and for Twitch to finish the VOD, rolls
`main` out in between, replays the day and prints the import line.
`scripts/import-big20.sh <vod_id>... [--replace-live] [--deploy]` lands them, and it is
**not** `import-vod.sh`: that one replaces a broadcast *day*, and he
practises in the afternoon of days he also runs Ninja Gaiden. This import is
additive and scoped to its own VOD. It imports only the runs that are not
the tracked game's — runs of the tracked game in the same pass are reported
and left for `import-vod.sh`, which knows how to replace a day of them — and
it re-counts the day's tracked-game runs afterwards and stops if any went
missing.

**`--replace-live` is for redoing a day.** By default the import adds, and a
run the live capture already has wins over the replay's. When the live bot
ran a build that has since been fixed — the week of 2026-09-08, whose live
capture saw six of the twenty games and named two of them wrong — that is
backwards: the flag deletes the live capture's practice rows inside the
VOD's span before the insert, so the replay's pass lands whole. Still never
a run of the tracked game, still never an Arcathlon row, and the numbering
of every game touched is recomputed.

**Rehearse with `LIVE=<a copy>` first.** Not optional. It is what caught the
marathon import that would have deleted 1888 runs, and it is what caught
this one's own version of the same bug: ownership was "a vod session holding
no run of the tracked game", which also selects an **Arcathlon** session for
the same VOD, whose ten completions are not the tracked game's either.
Ownership is now the session `tag` this import stamps, and a planted
marathon session on the same VOD is part of the rehearsal.

## Where the pane is

The LiveSplit window is top-anchored, so a game's timer sits as far down as
its split rows push it. Seven rows put it where the two Ninja Gaiden layouts
expect (y ≈ 820), and those games — Double Dragon II, Kid Klown, Steel
Legion, the Flintstones — recorded from the first day. One to four rows
leave it 100–180 px higher, with smaller digits, and the probe reaches
36 px; so Crisis Force, Uninvited, Faria, Monster Party, Parallel World,
Mega Man 6, New Ghostbusters II, Mini Putt and Yoshi were never even *seen*:
no lock, no pane pass, no title read, no run, and nothing in the session
events to say so. The obs log's parse rate is the only place it showed.

Measured with `locate` on VOD 2875828002 (2026-09-16). The timer's right
edge sat at x = 595–604 on every sample; only its top moved:

| rows | game | digits (x, y, w, h) |
|---|---|---|
| 1 | Mini Putt | 408, 560, 194, 70 |
| 1 | New Ghostbusters II | 437, 619, 167, 64 |
| 2 | Crisis Force | 359, 626, 245, 65 |
| 3 | Uninvited | 435, 666, 167, 65 |
| 4 | Faria … Mega Man 6 | 389, 677, 211, 59 |
| 4 | a wider-pitch pane | 296, 786, 299, 80 |
| 6 | Kid Klown | 410, 819, 189, 71 |
| 7 | Flintstones | 423, 829, 177, 66 |

`live.toml` carries four `[[layouts]]` for them — `big20-top`, `big20-short`,
`big20-mid` and `big20-tall` — each holding its class with 20 px of slack at
zero offset. `big20-tall` came last (2026-09-18): his Moon Crystal board,
eight segments with the big timer under them, is taller than the three were
measured for, and none of their pane crops reached its "Moon Crystal / Any%"
header, so no layout could name the board and an evening of attempts was
dropped at close. Its timer crop is the digits and their room to grow:
the timer is right-aligned and gains a digit on the LEFT at ten minutes
and at the hour, so the crop starts 30 px left of the pane's edge (`crop_x = 262`): the
older, wider practice pane of the first week puts "10:42" from x 285, and
the ink measurement ignores a pane border inside a crop. A crop 40 px
short of it read "10:04" as "0:04", a zeroed reset at 9:59 on every attempt
that got that far (five on the site, four more on the 18th); one 7 px
short clipped the leading digit, released the lock as poorly read, and a
candidate 36 px right read "0:11" for "10:11", a desync. And it is the digits' height only (`crop_y = 812`, 100 px): the
195 px crop before it took in two split rows, whose times were read as
the timer ("0:59.6" three seconds into an attempt, a desync every twenty
seconds), and it fit the older, shorter practice panes well enough to win
the lock there and read nothing. One
crop for the whole 560–736 span was tried first and was worse: with the
four-row panes at its bottom edge, Uninvited and Faria stopped recording.
Validated by replaying every unrecorded stretch of that VOD: Crisis Force
finished 11:25.8, Uninvited 12:51.4, Parallel World, Monster Party and Yoshi
all record, Kid Klown still records, and the Ninja Gaiden window is
row-for-row its baseline.

Two boards lock now and still record nothing, and both are **naming**
problems, not layout ones:

- **New Ghostbusters II** reads "New Ghostbusters" on many passes — the "II"
  is lost — and the roster's pool matcher keeps a trailing sequel number
  strict, so it refuses (`roster.rs`, "wide inside a roster, strict outside
  it"; that rule is what stops "Ninja Gaiden Ill" becoming Ninja Gaiden II).
  It could relax when the pool holds exactly one game of that stem, which
  is the case here and not for Zelda / Zelda II.
- **Mini Putt**'s one-row pane reads "Traditional" for its title — the
  course, or the category — and nothing on any roster is called that. Its
  time cell read "$:22" for 5:22 besides, the 5 as a dollar sign, which
  made the word part of the name; `board::glyph_repair` puts such a digit
  back (2026-09-18), and the title is the part that stays open.

A board that locks, convicts, and names nothing is what the drop rule in
`app.rs` is for (`close_would_fabricate`, see [detection.md](detection.md)):
a run that starts in the two-pass hysteresis window before suspension
starts as the tracked game, and with no target to carry would close as a
Ninja Gaiden reset carrying Mini Putt's attempt counter. It is dropped
instead. That path was unreachable until these layouts made the pane lock.

The other way round is caught too: a pane titled with one of the event's
own games, six split rows with times under it, is marathon-shaped, and with
one board entry configured the classifier's fallback took it for the
event's board — a lock the timer never judges, held while the tracker it
started found no roster and was started again on every pass, and an hour of
Celeste attempts unrecorded. A title that names a roster game is a practice
pane, never the board (`marathon::classify`).

The robust version of all this is not more crops: it is finding the pane
when no layout fits — what `locate` does in one frame — and locking there.
The decoded frame is the union of the configured rectangles, which the
three layouts now make large enough to hold every pane seen; the finder
would have to run inside it.

## Reading the result

`/big20/` on the site (`site/big20.html`, built by `build-site.sh` from the
report's `big20` block, which carries every practice attempt per game) is the
twenty games **in race order**, each linking to its own page, with the goal
it must be finished to, attempts and finishes (and the rate), his best and
when it was set, his latest finish and how far off the best it was, a trend
of every attempt (finishes as points, a new best in green, resets as ticks at
the depth the timer reached), when he last practised it, and — in a column of
its own — his time for that game in one of his Arcathlons. Above the table:
the **sum of his bests** and the **sum of his latest finishes**, each over
the games that have a finish with the rest named, because the race is the
twenty back to back and a sum over nineteen is not a race time. Below it,
the practice days: games touched, attempts, finishes, time on the timer, and
the bests set that day (a first finish named as such, an improvement with
what it came down from).

A game's own page (`/game/<slug>/`, `site/event.html`) shows the same figures
for that game as tiles, one chart of every running of the game in time
order — practice attempts (finishes as points, resets as bars), the game's
segment in each full practice run (squares) and each Arcathlon completion
(rings), on one time scale with his best Arcathlon time as a dashed rule —
and the attempts by day with a bar for how far each got on one scale for
the page. Each attempt carries LiveSplit's own number where
the bot read it off that game's counter (the bot's ordinal, "#n", where it
did not), and the attempts tile says the range his counter ran over: the
gap between that range and the attempts recorded is what the capture
missed. The counter is per splits file, so each game is its own sequence
(`docs/detection.md`, *Splits, run numbers and golds*). His marathon completions of the game are listed
apart at the end and are in none of the figures.
Those last are kept apart deliberately: six of the twenty are also Arcathlon
games, and a completion from a marathon in July says something real about
the game but is not practice for this race.

The empty rows are the point. A leaderboard shows what has been done; a prep
page has to show what is left.

The landing page carries the same block in miniature while the race is what
he is doing: a card under the live panel and above the Ninja Gaiden records
with the date, how many of the twenty are finished and practised, the
attempt count, a twenty-cell strip in race order (gold for finished, faint
for practised) and the games he has touched today. It shows itself while
the race is ahead (until three days after it) and his newest practice run is
under two weeks old; when he is back to grinding Ninja Gaiden it goes away
by itself and the page reads as before.

## Full practice runs on the race board

Each run has a page of its own at `/big20/runs/<day>/` (`site/big20-run.html`,
"-2" for a second run on the same day): the twenty games in race order with
the game's time in that run, how it sits against his practice best and his
best for the game in any full run, the transition before it and the clock at
its finish, under a chart of the clock through the run against the staircase
of his practice bests and of his best run. The prep page's practice-runs
table and the runs page's column headers link to them.

From 2026-09-17 he also runs the whole race on its own splits ("Big 20 #23
/ Practice Run"). That is a marathon board, tracked by its rows, and it has
its own section in [marathons](marathons.md#the-race-board-big-20): the
replay script, the bracketed transition rows, the rows placed by the
roster's order, and how the live bot locks on it. Its rows are filed under
`Big 20 #23 run`, apart from the per-game practice attempts.

On the site those runs are the **practice runs** section of `/big20/`: how
many, the best and its distance above the sum of bests; a chart of each
run's clock against the sum-of-bests line (a run cut short is a hollow
point labelled with how far it got); and a table, newest first, with the
clock at the last game, the time in the games and between them, how the run
sat against his practice bests over the games it reached, and where that
went — the two games that cost most and the two where he beat his practice
best inside the run. A run in progress sits on top with the clock at its
last game, read from the report's live panel (`now.marathon`). `/big20/runs/`
(`site/big20-runs.html`) is the twenty games down the side and one column
per run, the fastest per game marked once he has run it more than once.

The report carries them as `big20.run_throughs[]` — `day`, `started_at_ms`,
`games`, `reached_ms`, `segments_ms` and `segments[{game, ms, cum}]` in the
order he reached them — grouped by one rule (`report.rs`): a run starts
where the marathon clock had nothing before the row, its cumulative being
its own segment (the first game of a fresh set of splits), or after two
hours away from the board. Not by session — a bot restart mid-run opens a
new one, and grouping by session cut one evening's run into three — and not
by the clock stepping backwards between neighbouring rows: a row is filed a
pass or two after it ends, sometimes twenty minutes after when its cells
read badly, and then sorts after a row that ended later on the clock; that
rule cut the second run into three on the page. A row filed out of order
stays in its run, in clock order. A new kind of page has to be added to
`deploy-site.sh`'s explicit page list or it silently never ships;
`/big20/*` is one invalidation.

## The next race

A roster edit, not a code change. `assets/big20-roster.toml` carries the
race's `name`, `games` (in race order), `goals` (matched **by position** —
a list of a different length is refused rather than silently attaching every
goal to the wrong game), `board` (what his splits print for each game, by
position too: "Flintstones", "Kid Klown", "Celeste Mario" — the rows are
matched against these and the full names both, the runs filed under the
full names), `ordered = true` (the games are run and printed in this order,
so the tracker places every row of the board by its position in the list),
`url`, `date` and `category`. Point
`roster::BIG20_RACE` at the new event name and the prep page follows.

Keep it a separate file from `assets/arcathlon-rosters.toml`.
`Rosters::identify` picks the event most of a board's names fit and the pool
fallback assumes each game belongs to exactly one event; Mega Man 6 is in
both Arcathlon #9 and this race, so merging the files would break that
assumption for every board either way.
