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

**Placed by the roster, not the window.** The race is run in a fixed order
and the board prints it so, two rows a game (the game, its category in
brackets under it) in a window that scrolls with the last row pinned. An
event marked `ordered = true` in its roster file has every row placed by its
game's position in the list — the game at slot 2k, its category row at
2k + 1 — from the names on each pass, never from where the row sits in the
window (`Marathon::align_ordered`). Rows the names do not place take the
slot between the placed rows either side where the spacing agrees, and a
bracketed row directly under a placed game is its category row. Nothing
scrolls, nothing is superseded, and the pinned last row has one slot from
the first pass on. Before this, the pinned row moved slots with every
scroll and a nameless transition row was filed under its stale name as a
0:30 "Moon Crystal"; a transition row that never got a slot put the
arithmetic of the rows either side off by exactly its length. Until the
event is identified (a file of one event needs one row naming one of its
games) nothing is placed at all.

**The board's own spellings.** The roster's `board = [...]` list, by
position like `goals`, is what his splits print for each game: "Flintstones",
"Kid Klown", "Celeste Mario". A row is matched against both the board
spelling and the full name, the better fit winning and a tie going to the
board spelling ("World" is Parallel World's row, not the tail of Kid
Klown's name). And on an ordered board the slot itself says what a row's
run is filed under — the even slots are the games in order, the odd ones
their category rows, never filed as games whatever their name came back as
("(Any%)" read "au" and its 0:30 went in as a game before this), and
recorded whether or not it came back at all: a category row whose name
never reads still ends at the cumulative the row under it is measured
from, and left unrecorded it breaks the chain for every row below (four
games of one twenty-game run, the board showing every one) — so a
row that read as nothing any roster folds ("4ydiide Aly?" was Hydlide's)
is still filed as its game; the reading stays in the log. Before this
"Funtstones", "Kid Kiown" and "World" were filed as read.

**What the total is worth.** The total is bounded by the board: a reading
more than an hour past the largest time any cell has shown, comparisons
included, is not this clock and is ignored ("7 4:24:11" off the crop's
edge parsed as 74:24:11 on a quarter of the frames, and the monotone clock
followed it for four hours; the parser now drops a lone digit in front of
a time that has its hour, and refuses a two-digit hour past 23). The log
says when a total is ignored (`tracker-total` in `healthcheck.sh` at five
refused passes: the timer is being misread), and says once per row when a
settled, coherent cumulative has gone five passes with neither the total
nor the row above to vouch for it ("nothing vouches for it"), and again
when such a row is filed after all ("filed after N unvouched passes") — a
row said and not filed within ten minutes is a lost anchor or a wrong
total, `healthcheck.sh` raises it as `tracker-unvouched`, and the
session-close line lists the rows still unfiled.

**A first time under a watched row.** At 480p the board's "-" cells often
do not read, so a game's row carries no vote at all until its time
appears — and a first reading with a time was a baseline, "finished before
the bot looked". Under a row THIS tracker watched finish (not one recorded
from the database after a restart), and exceeding it, a first time is a
completion to judge instead. Crisis Force, "11:30 / 51:59" on four passes
under a recorded 40:28, was never filed for this.

**The hour carried down.** A cumulative reading as minutes and seconds
under a recorded row past the hour has lost its hour digit — the column is
monotone down the board — and takes the hour from the nearest recorded row
above (one more where that still leaves it short). With nothing recorded
above — a tracker started mid-run — the hour is the one that puts the
reading nearest the row's own comparison, or the row above's. On the
finish board every total read "33:41" for 4:33:41 for ten passes running.

**Filed from the row below.** At 480p the theme trades 5, 6 and 8 for each
other, so a game's cumulative can come back "1:38:25", "1:35:28", "1:35:25"
on successive passes and never twice the same — and a cumulative that never
settles is never a completion. The row under it is the transition into the
next game: half a minute long, read the same on every pass, and its
cumulative includes the game. So once a row is recorded with a settled
segment beside it, the tracker files the unrecorded row above it at the
recorded cumulative less that segment, bottom up, each recorded row
answering for the one above. The segment is the row's own column where it
agrees with the arithmetic to within two seconds (a derived value carries
two roundings), the arithmetic where the row above is recorded too and the
column disagrees. Only a row the tracker first saw EMPTY is filed this way:
a row already carrying its time when the board appeared was finished before
the bot looked, and stays unrecorded, as it always has. The log line says
"filed from the row below" and the completion carries `backfilled`.
Measured before the rule, a full run of the twenty filed four games by the
columns alone, with the transition rows read cleanly all the way down.

**Why a row was or was not filed.** `NG_MARATHON_TRACE=<part of a row name,
or a slot number, or all>` prints, on every pass, each cumulative an
unrecorded row has voted for and every guard's answer (settled, coherent,
denied by its segment, vouched by the total, vouched by the arithmetic),
with the row's baseline. Run it under `audit --dir` over a board log
(`boards-<vod>.jsonl` + `obs-<vod>.jsonl`): the tracker is deterministic
over its inputs, so a game that went missing in a replay can be asked why
in a second, without the replay. That loop is what found every rule above.

**Comparisons.** From the second run on, his splits carry the previous run
as the comparison, so every unrun row shows a time from the first pass — a
baseline, as on an Arcathlon board — and three rules keep a comparison from
being taken for a completion. A comparison read with and without its hour
digit ("12:28" for 1:12:28) is one baseline, kept under the reading that
has the hour, and a reading that is the baseline without its hour clears
the row's votes as the baseline does. "Just now" — a row arriving with its
time where the total stands — speaks only for a row that has never yet
shown a time: the unrun rows were on the board with their comparisons all
along, and a tracker started with the total at 51:00 filed Crisis Force's
51:59 comparison on its second pass before this. And a candidate the total
ALONE vouches for (nothing recorded above it, no settled segment agreeing
with the arithmetic) is refused when it is the row's own comparison with
one digit read wrong: the pinned last row's 5:04:57 read "4:04:57", the
clock passed 4:05 with the row's neighbour still unrun, and yesterday's
Moon Crystal was filed as today's — and today's, at 4:38:17, refused as
already recorded. A segment under half his own best for the game is a
misread, not a run: that candidate is refused and the next most voted one
is tried on the same pass, so a misreading read once more than the real
completion cannot hold the row.

**The segment column's word.** The column refuses a candidate only when no
settled reading of it agrees with the arithmetic (the comparison case:
"20:34 throughout" where 16:16 was wanted). Any settled disagreement used
to refuse, and then the strongest reading did; at 480p a segment of 11:14
came back "13:14" on four passes and "12:14" on five beside twelve of
"11:14", and 11:16 came back "13:16" six times against two of "11:16" —
in both the cumulative was right on every pass and the delta column agreed
with it. Where the column's strongest reading still disagrees, `segment_for`
waits its patience out and files the arithmetic's value, marked derived.

**After a restart.** A rollout mid-run (three of them on 2026-09-18) hands
the new tracker a board where the games already finished are baselines,
not completions it watched, so nothing above the runner is recorded and
the next completion had only the total's three-minute window — a
cumulative read three ways in three minutes missed it. On an ordered board
a settled baseline of a row above that has never been followed by a
settled other reading, and that is behind the clock, is a finished row's
real cumulative and anchors the arithmetic for the row below (Steel Legion
14:03 against the 1:31:45 above it). Behind the clock matters: a baseline
ahead of it is the comparison of a row not yet reached, and without that
check Faria's 2:01:38 vouched for Monster Party's 2:15:10 from the day
before. The games finished between two restarts are still lost live; the
replay from second 0 is what files them.

**What the timer is worth here.** The marathon total is the timer read off
the big clock, and it is the only witness for a row nothing above it
anchors. Two things made it worth more. The clock is right-aligned and
grows LEFT with the hours: the `big20-race` timer crop that fitted "10:18.3"
cut the hour digit of "3:55:28.3" four hours in, tesseract read "155:09.3",
and the total was hours from the truth on the frames it parsed at all. Widened
to start at 320, the crop then began over the pane's corner ornament, which
tesseract read as a "7" in front of the time ("7 4:24:11", 74 hours) on a
quarter of the frames, and it ended before the hundredths; it now starts at
the first digit and ends past them (`live.toml`, `crop_x = 376`, `crop_w =
236`: on the same two minutes of footage, prefixed reads 224 to 3, parsed
frames 232 to 803). And with a marathon
in force the frame loop takes readings the timer parser declines
(`timeparse::parse_marathon_total`): a lost tenths digit with its separator
kept ("15:08:", "3:55:28."), a colon read as a point ("15.16" for 15:16 —
a total under a minute is not a reading of anything, so the point can only
have been the colon), bare digits ("1526"). The tracked game's own timer is
untouched. The board's cells get one repair of their own: a digit read as
the letter it looks like ("$:22" for 5:22, "18:S2" for 18:52) is put back
where the word has a colon and the result is time-shaped
(`board::glyph_repair`) — left as it was, "$:22" glued onto the name and
the row read as one cell, which is no reading at all; Mini Putt was filed
seventeen minutes late for it.

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
  `big20-race` layout whose splits crop takes the names column in and whose timer crop is wide enough for the clock past the hour (see "What the timer is worth here"). The pane binarises differently per theme — at the deployment's
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
