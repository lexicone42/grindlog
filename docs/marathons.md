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
