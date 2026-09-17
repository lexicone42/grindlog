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

`scripts/import-big20.sh <vod_id>... [--deploy]` lands them, and it is
**not** `import-vod.sh`: that one replaces a broadcast *day*, and he
practises in the afternoon of days he also runs Ninja Gaiden. This import is
additive and scoped to its own VOD. It imports only the runs that are not
the tracked game's — runs of the tracked game in the same pass are reported
and left for `import-vod.sh`, which knows how to replace a day of them — and
it re-counts the day's tracked-game runs afterwards and stops if any went
missing.

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

`live.toml` carries three `[[layouts]]` for them — `big20-top`, `big20-short`,
`big20-mid` — each holding its class with 20 px of slack at zero offset. One
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
  course, or the category — and nothing on any roster is called that.

A board that locks, convicts, and names nothing is what the drop rule in
`app.rs` is for (`close_would_fabricate`, see [detection.md](detection.md)):
a run that starts in the two-pass hysteresis window before suspension
starts as the tracked game, and with no target to carry would close as a
Ninja Gaiden reset carrying Mini Putt's attempt counter. It is dropped
instead. That path was unreachable until these layouts made the pane lock.

The robust version of all this is not more crops: it is finding the pane
when no layout fits — what `locate` does in one frame — and locking there.
The decoded frame is the union of the configured rectangles, which the
three layouts now make large enough to hold every pane seen; the finder
would have to run inside it.

## Reading the result

`/big20/` on the site (`site/big20.html`, built by `build-site.sh` from the
report's `big20` block) is the twenty games **in race order**, each with the
goal it must be finished to, how many attempts and finishes, his best, and —
in a column of its own — his time for that game in one of his Arcathlons.
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

## The next race

A roster edit, not a code change. `assets/big20-roster.toml` carries the
race's `name`, `games` (in race order), `goals` (matched **by position** —
a list of a different length is refused rather than silently attaching every
goal to the wrong game), `url`, `date` and `category`. Point
`roster::BIG20_RACE` at the new event name and the prep page follows.

Keep it a separate file from `assets/arcathlon-rosters.toml`.
`Rosters::identify` picks the event most of a board's names fit and the pool
fallback assumes each game belongs to exactly one event; Mega Man 6 is in
both Arcathlon #9 and this race, so merging the files would break that
assumption for every board either way.
