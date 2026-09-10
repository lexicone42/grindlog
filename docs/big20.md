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
