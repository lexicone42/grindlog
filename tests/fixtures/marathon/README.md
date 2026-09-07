# Marathon fixtures

Three whole marathon broadcasts, pane pass by pane pass, for the tracker in
`src/marathon.rs`. Each JSON is the board as `board::read_board` actually
returned it on every pass of the day, with the marathon total the timer had
last given within the previous 30 s — which is exactly what the run loop
hands `Marathon::observe` — and the ten games of that event from an answer
key read off the VOD by hand and verified twice.

They exist because the synthetic tests beside them cannot fail the way a real
board does. The first run of these found four defects that every synthetic
test passed: one eleven-row pass (the pane's bottom edge picking up the line
of text under it) growing the slot list and stranding six games for two
hours; a game finishing just as the board filled out and being read as a
comparison time; a row still being played outvoting its own result with its
segment column; and a stray mark on a first letter splitting a row's votes
between two spellings.

| fixture | broadcast | what makes it hard |
|---|---|---|
| `rand-2858870362` | VOD 2858870362, Aug 28, "Randomized Arcathlon", 4h20m, 309 passes | no comparison times anywhere: undrawn games are "???" with "-" in both columns, and a row does not exist until it has a time, so the first game appears out of nothing with its result already in it. One pass returned eleven rows. Captured with a pane crop that cut the TITLE row off, so the title reads as the first game's name or nothing all day — which is why this is the one fixture whose first game is not recovered (see below) |
| `rand-2833684629` | VOD 2833684629, Jul 31, "Randomized Arcathlon", 7h35m, 303 passes | he opens on the PREVIOUS event's splits — ten rows of "???" over last week's times — so every slot starts with a baseline and a nameless row; the pane then shows only the row he is playing, so passes come back with one row where the board has ten; one row's 15:52 reads "18:52" on every other pass; the footer leaks in as an eleventh row; and two of the day's games are "Ninja Gaiden III" and "Ninja Gaiden II", close enough that the signature groups them as one label and reads the board as a RUN board for twenty passes running |
| `num-2830524439` | VOD 2830524439, Jul 27, "Arcathlon #6", 5h20m, 347 passes | the pane is transparent over the game, so readings are thinner; all ten comparison times are printed from the first frame, and a game is finished only when its cumulative CHANGES from one of them. He switches to his Ninja Gaiden layout at the end, so the last 50 passes are another board entirely |

Fields per pass: `t_ms` (into the broadcast), `total_ms` (the marathon total,
null where the timer went unread), `title` (null on most passes — these were
captured with a pane crop that cut the title row off, which is why the
tracker is not allowed to need it), and `rows` of `{name, cells}` exactly as
the board reader returned them, junk and all. `expect` is the answer key:
`order`, `game`, `segment`, `cumulative`, `ended_s`, and `recorded_as` where
OCR does not give the board's own spelling ("Hammerin’ Harry" for
"Hammerin' Harry").

The fixtures are replayed through the decision sequence `app::track_marathon`
runs — `classify`, the let-go count for a board that reads as somebody
else's, the reconcile against what is already recorded, then `observe` —
rather than straight into `observe`, so what they test is the path the bot
takes. A `Vec<i64>` of the cumulatives recorded so far stands in for
`db::marathon_totals`; no database is involved.

The tests assert every game found, every segment time equal to the board's
own, every name EXACTLY the spelling the key says the tracker settles on
(runs are filed under that string and `runs.game` groups by it, so a fuzzy
comparison would pass a change that silently splits a game's history), no
completion the answer key does not have, and each recorded instant within a
stated distance of the key's — 300 s on `rand-2858870362`, 600 s on
`rand-2833684629` and 360 s on the numbered day, where a game's row went
unreadable for a few passes.
`replays_a_broadcast_the_bot_restarted_in_the_middle_of` replays one of them
with the tracker dropped at a pass in the middle and picked up again from
the database alone, which is what a crash, a rollout or a stream reconnect
does to the live bot, and asserts the same ten games come out and no
eleventh. Run them with `cargo test --release replays_a_ -- --nocapture` to
see the tables.

`rand-2858870362` is checked for nine of its ten games, not ten. Its first
game, Astyanax, finishes while its row is the only one on the board, so the
rows cannot yet say what the board is; the title could, and in this capture
there is none. The same broadcast replayed whole with the pane crop
`scripts/replay-arcathlon.sh` uses — title row included — records all ten.

To rebuild one: replay the VOD with `scripts/replay-arcathlon.sh` (or any
config with `debug.board_log` and `debug.obs_log` on), then join the two logs
— every board pass, plus the last `parsed_ms` within the 30 s before it — and
wrap them around the answer key's ten games.
