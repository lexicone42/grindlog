# Marathon days

The streamer runs "Arcathlon" marathons: ten NES games back to back on
a different OBS scene, tracked by which board rows have completed rather
than by the timer. They have their own replay, their own import and their
own audit, and every one of those differs from the Ninja Gaiden path in a
way that has already caused damage once.

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

`scripts/import-arcathlon.sh` lands those games in the live database, and it
is **not** `import-vod.sh`: that one replaces a broadcast *day*, which is
right for a Ninja Gaiden VOD and destructive here, because he has run Ninja
Gaiden in the morning and a marathon in the afternoon of the same day (2026
-07-23 has two VODs and 45 Ninja Gaiden runs the marathon import must not
touch). The marathon import is additive and scoped to its own VOD by the
session's `vod_id`, and it re-counts the day's other categories afterwards
and stops if any went missing. Rehearse it with `LIVE=<a copy>` first.

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

## The race board (Big 20)

The Big 20 race — and his full practice runs of it, from 2026-09-17 — is
the same pane in the same place with twenty games instead of ten, and each
game as **two rows**: the game (`01 - Die Hard`) and its category in
brackets under it (`(Any% Beginner)`), which is the transition into the next
game. The board **scrolls**: LiveSplit shows a window of nine or so rows
with the last pinned, and moves it down as he plays.

`scripts/replay-big20-race.sh <vod_id>` is `replay-arcathlon.sh` with the
race roster (`assets/big20-roster.toml`) and a board entry matching the
board's title; it writes `big20-race-db/vod-<id>.db`, one row per completed
game under category `Big 20 #23 run`. Land it with
`BIG20_OUT=big20-race-db BIG20_TAG=big20-race ./scripts/import-big20.sh`,
tagged apart from the same VOD's practice import. Three things the tracker
does for this board that an Arcathlon never needed:

- **A bracketed row is a segment, not a game.** It is filed under nothing,
  but its cumulative stands, because the row under it derives its segment
  from it: Pac-Mania's 7:26 is 10:18 less the 2:52 the bracketed row
  reached. The mark is a name that starts with a bracket, or ends with one
  and has no opening bracket anywhere (OCR loses the opening one far more
  often); "SMB3 (Warpless)" carries its own and is a game.
- **Rows that scroll into view are tracked.** Rows are placed by name where
  a name anchors a slot; under the last anchor they continue positionally
  into new slots — but only where the first anchored row sits at least two
  slots below its position, one game of this board, which a fixed board
  never shows. (Continued unconditionally, the Arcathlon audit recorded a
  game never played: the pane's footer read as an eleventh row.) Rows that
  scroll off keep their slot and its record. An anchor has to keep the
  board's spacing: LiveSplit PINS the last row ("20 - Moon Crystal") at the
  foot of the window whatever scrolls above it, and its early, low slot
  would otherwise anchor the bottom of every pass and leave the games
  scrolling in above it unplaced — it is treated as a row that has moved,
  taking the next slot and leaving its stale one cleared, which is also what
  stopped a bracketed row's transition time landing there as "Moon Crystal
  finished in 0:30". And a tracker is only rebuilt on three CONSECUTIVE
  passes that disown its board: one pass in five or six on this board comes
  back with its names damaged past matching, and counted across the good
  passes between them, three such passes rebuilt the tracker every minute of
  the first live run.
- **Live, the layout locks on the board, not the timer.** The timer's digits
  sit between the decimal point and a logo with pixels to spare on neither
  side; no crop reads them without cutting a digit or taking the logo in,
  and the probe refuses both. While unlocked, and only where the
  configuration tracks some board by its rows, one layout at a time is read
  as a pane every ten seconds; a pane that classifies as a board-mode board
  twice running grants that layout the lock through the same path a timer
  does. A board-granted lock is exempt from the timer's dark/poor-quality
  unlocks, feeds the run state machine nothing, and is let go when no
  marathon is in force five minutes after the grant. `live.toml` carries
  the entry (`[[games]] name = "Big 20 #23 run", mode = "board"`) and the
  `big20-race` layout whose splits crop takes the names column in. The pane binarises differently per theme — at the deployment's
  `[splits] threshold` (150) the race board reads one to four rows a pass,
  at 100 ten, and at 100 his Ninja Gaiden pane's title reads as noise and
  the gate convicts its own board — so the board probe tries the configured
  threshold and then 100, remembers which found the board, and the pane
  pass reads at that threshold only while the lock is board-granted. The
  probe tries every threshold and keeps the reading with the most rows THAT
  HAVE NAMES (150 gives the race board eight rows off their time cells with
  no readable names; 100 gives ten, named), and it keeps running at a third
  of its cadence while the timer holds the lock, because the default crop
  lies over this board's time cells and locks on them as a timer first.
  Once the board holds the lock, the timer candidates do not compete for
  it, the layout's configured rectangles stand (no measured geometry is
  adopted), and every pane pass reads only that layout's own rectangle —
  the union of every layout's crops is what ffmpeg decodes, and the game
  screen's edge and the sprites in it bent tesseract's page analysis over
  the whole image: "2:01:07" came back "01:07" with the "2" in the name
  column, and the same rows read clean off the pane alone.
