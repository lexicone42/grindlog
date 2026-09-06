# Marathon fixtures

Two whole marathon broadcasts, pane pass by pane pass, for the tracker in
`src/marathon.rs`. Each JSON is the board as `board::read_board` actually
returned it on every pass of the day, with the marathon total the timer had
last given within the previous 30 s — which is exactly what the run loop
hands `Marathon::observe` — and the ten games of that event from an answer
key read off the VOD by hand and verified twice.

They exist because the synthetic tests beside them cannot fail the way a real
board does. The first run of these two found four defects that every
synthetic test passed: one eleven-row pass (the pane's bottom edge picking up
the line of text under it) growing the slot list and stranding six games for
two hours; a game finishing just as the board filled out and being read as a
comparison time; a row still being played outvoting its own result with its
segment column; and a stray mark on a first letter splitting a row's votes
between two spellings.

| fixture | broadcast | what makes it hard |
|---|---|---|
| `rand-2858870362` | VOD 2858870362, Aug 28, "Randomized Arcathlon", 4h20m, 309 passes | no comparison times anywhere: undrawn games are "???" with "-" in both columns, and a row does not exist until it has a time, so the first game appears out of nothing with its result already in it. One pass returned eleven rows |
| `num-2830524439` | VOD 2830524439, Jul 27, "Arcathlon #6", 5h20m, 347 passes | the pane is transparent over the game, so readings are thinner; all ten comparison times are printed from the first frame, and a game is finished only when its cumulative CHANGES from one of them. He switches to his Ninja Gaiden layout at the end, so the last 50 passes are another board entirely |

Fields per pass: `t_ms` (into the broadcast), `total_ms` (the marathon total,
null where the timer went unread), `title` (null on most passes — these were
captured with a pane crop that cut the title row off, which is why the
tracker is not allowed to need it), and `rows` of `{name, cells}` exactly as
the board reader returned them, junk and all. `expect` is the answer key:
`order`, `game`, `segment`, `cumulative`, `ended_s`.

The tests assert every game found, every segment time equal to the board's
own, every name matching by `game_matches` (OCR takes a letter off "Little
Samson"), no completion the answer key does not have, and each recorded
instant within a stated distance of the key's — 120 s on the randomized day,
360 s on the numbered one, where one game's row went unreadable for a few
passes. Run them with `cargo test --release replays_a_whole -- --nocapture`
to see the table.

To rebuild one: replay the VOD with `scripts/replay-arcathlon.sh` (or any
config with `debug.board_log` and `debug.obs_log` on), then join the two logs
— every board pass, plus the last `parsed_ms` within the 30 s before it — and
wrap them around the answer key's ten games.
