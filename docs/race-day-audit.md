# Race-day audit, 2026-09-29

Five independent readers went over the tracker, the board reader, the run loop, the scripts and what differs on race day; every finding was then attacked by two skeptics, one on the code and one on the corpus under `race-audit/`, and the survivors ranked by what they would cost on the public site on 2026-10-10. Thirty-eight findings, thirty-six survived a skeptic. Line numbers are as of commit d6870eb. The list is the work plan for the last week before the race; strike items here as they land.




Ranked by what it costs on the public site on race day. Duplicates naming one root cause are merged (originals noted). Severity is the two skeptics' votes as given.

**Do this first, regardless of rank:** finding 16 (the audit harness's `hits` reset). Until it lands, `scripts/audit-race.sh` scores every tracker change against a rebuild policy the live bot does not run, and the 2026-09-25-vod proof for several fixes below is the harness artefact, not the bot.

---

## 1. The race starts before the bot's active window; the pre-09:40 offline poll is every 30 minutes
`src/capture.rs:56` (`current_offline_poll`), `live.toml:49-51`
thebig20nes.com/race-23 says 12pm Eastern = 09:00 PDT; `active_hours` starts 09:50 so `lead` is 09:40, and before that the box sleeps `quiet_poll_secs` = 1800 s at whatever phase it inherited from the previous evening's offline detection (today's log: 08:01/08:31/09:01). A go-live at 09:00 is noticed as late as 09:31, then HLS start, two board-probe hits and three take-up passes follow, so Die Hard (2:17) is a baseline on every phase and Pac-Mania/Double Dragon II are too on a bad one; `vote()` never harvests a `Baseline::Was` row and no health signal fires (polling is green "outside active hours").
Severity: loses-rows-on-race-day / loses-rows-on-race-day.
**Fix:** set `active_hours = ["07:30","20:30"]` in `live.toml` for race day (existing field, no binary change) and do the approved plain-`kill` restart the evening before; the "checking again in Ns" log line then shows the 60 s cadence in force before 09:00. No corpus day can show this (it is capture, not the tracker); the proof is `logs/live.log` on the morning of 2026-10-10 showing a 60 s poll at go-live. Corpus shape of the loss itself: vod-2876668056 (board first seen at t=456 s, Die Hard already 2:22/2:22, expected.txt starts at slot 3).

## 2. Backfill ("filed from the row below") is dead on a board with comparisons: Jaws lost on 2026-09-29-live, live and in the replay
`src/marathon.rs:1669` (gate `baseline != Baseline::Empty`), with `:980`/`:1274` absorbing the category row
Every row on a comparison board settles `Baseline::Was(comparison)`, so `backfill()` skips every row and `harvest` can only file a cumulative the board printed; Jaws' 3:51:11 was read sixteen ways at 480p and never once correctly, while Moon Crystal below it was recorded with a settled 13:02 segment — exactly the input the backfill rule was written for. Its category row (slot 37) is doubly blocked: the runner sat on Jaws for 13 minutes, so its real 3:51:41 went into `baseline_votes` and became "what it always showed" (1274), and its segment votes are wiped every pass, so unblocking 1669 alone files slot 37 but `best_segment` still returns `None` for Jaws.
Severity: loses-rows-on-race-day / loses-rows-sometimes. One game in ~6 comparison-board full runs, permanent (the VOD replay uses the same reader).
**Fix:** let `backfill()` walk a `Was`-baseline row once the runner has passed it and the row below is recorded with a settled segment, and let an odd slot's segment fall back to `baseline_segment_ms` (the fixed 0:30) when its votes were absorbed. Proof: `scripts/audit-race.sh` on 2026-09-29-live gains `37:13871/698` (Jaws) and the day reads 20; all other days byte-identical.

## 3. `import-big20.sh --replace-live` (the command after-broadcast.sh prints) deletes live-caught race rows the replay did not file
**Landed (per-game merge, then per-run: a live row goes only where the replay has an overlapping run or the same finish on the same day; kept rows are named).** The per-game version still lost the two late Steel Legion attempts of 2026-09-28 the replay never saw.
`scripts/import-big20.sh:118-120, 166-168`; printed by `scripts/after-broadcast.sh:89`
LIVE_ROWS is every hls run in the VOD span that is not Ninja Gaiden/Arcathlon — all 'Big 20 #23 run' and 'Big 20 #23' rows — and it is deleted wholesale before the replay's rows are inserted, with nothing compared game by game. On 2026-09-28 live filed Steel Legion (884 s) and the replay did not (it filed Parallel World instead); rehearsed on a copy, the day keeps 18/20 so nothing looks wrong, Steel Legion vanishes, three Steel Legion practice attempts (incl. the 758 s best) go with it, and attempt numbers close over the gap.
Severity: loses-rows-sometimes / loses-rows-on-race-day.
**Fix:** delete a live row only where the replay has a row for the same game and category on that day, and print the per-game diff (live-only, replay-only) before any delete. Proof: `LIVE=<copy> BIG20_OUT=big20-db-2026-09-28 scripts/import-big20.sh --replace-live 2886471637` keeps Steel Legion and adds Parallel World (20 distinct games); not an audit-race.sh case.

## 4. rollout.sh's "wait until idle" gate cannot see a marathon: a rollout during the race SIGTERMs the tracker at once
`scripts/rollout.sh:95` (merged: two findings, same line)
Under a board-granted lock the run state machine is fed nothing (`app.rs:3837-3841`), so every obs line is `"phase":"IDLE"` (2026-09-29-live: 39,310/39,310) and the wait loop breaks on its first check. `logs/live.log` 2026-09-18 shows three SIGTERMs mid-run (18:31, 19:06, 19:46), session 224 filing nothing in 34 minutes, and the live pass closing 13 games against the VOD's 20; every restart is also the trigger for findings 5, 6 and 9 below. (The finder's 2026-09-25 example was wrong: that SIGTERM landed four minutes after "marathon over".)
Severity: loses-rows-on-race-day / loses-rows-sometimes (needs an operator to run it; it happened three times in one run).
**Fix:** in the wait loop also refuse while the last obs line's `layout` is a board lock (`big20-race`) or `logs/live.log`'s newest "marathon board:" line has no later "marathon over"; healthcheck.sh:261 already has the sqlite query for the marathon event. Proof: run the gate's check against `obs-live.jsonl` during any practice broadcast and see it wait; `bash -n scripts/rollout.sh`.

## 5. A SIGTERM or a confirmed Twitch offline while he is −10 s to +60 s against the comparison's segment files the previous run's split as his finish, then the real finish as a duplicate
`src/marathon.rs:1535` (`close()` window), fed by `:992` (`held_segment_ms`) (merged: three findings)
On the row being run the segment cell shows the comparison's segment and `held_segment_ms` stores it on every moving-delta pass with no baseline check; `close()` files `prev + held` whenever it lies in `[total − 60 s, total + 10 s]`, which while behind pace is the ten seconds before and the whole minute after the split he is chasing. A truncation sweep over the corpus (14,856 cut points) fires this on 1.3% of in-force passes (67 fabricated rows on 11 days: 2026-09-29-live pass 109 files Uninvited 12:57 at 1:24:31, the 09-28 time, against the real 13:23 at 1:25:28); the restarted tracker seeds `known` with the phantom cumulative, so the real finish is not `known` and is filed too, or becomes a baseline and the wrong time stands alone.
Severity: loses-rows-sometimes / loses-rows-sometimes (both votes on all three originals).
**Fix:** in `close()` refuse a held segment whose pass read the row's baseline cumulative beside it (or whose segment equals `baseline_segment_ms`), since a finish pass differs from a mid-run pass exactly there. Proof: `head -109 race-audit/boards-2026-09-29-live.jsonl` plus the day's obs → audit files nothing at the close; the full `audit-race.sh` sweep stays byte-identical (2026-09-24 still files Moon Crystal at the close, its last pass's cumulative is not the comparison).

## 6. A rebuilt/seeded tracker cannot file any game that finished before it looked but was not yet in the database, and the runner can pin below the real one
`src/marathon.rs:1008-1013` (runner from `rposition(recorded)`), `:1149`/`:1259` (`already` only on exact baseline settle), `:1159` (`chained` needs `recorded_at_ms`) (merged: two findings)
After a restart (rollout, StreamOffline re-take-up, disowns rebuild) `seed()` fills `known` only; a slot is marked recorded only when its baseline settles on an exact known value, so a top row that misreads (Flintstones 11473 read 12793/13333/12073) is never marked while a lower clean row (Mini Putt) is, the runner pins two slots below it, and every finish under it is `under` → baseline `Was(finish)` forever (Hydlide 12326, Yoshi 12772, Celeste 13549 on 2026-09-25-vod, finishing 2–23 min after the rebuild). Independently, a row finished 1–5 min before the restart and still settling is a baseline on the new tracker's first pass (`arriving` false, `chained` impossible): this lost Moon Crystal live on 2026-09-22 (stream drop at 21:26:47, VOD 2881366326 seeds 19, slot 38 settles `Was(15659000)`).
Severity: loses-rows-sometimes / loses-rows-sometimes (both originals). Every seeded take-up in live.log predates the runner rule (#142), so the interplay has never run live.
**Fix:** mark a seeded slot `recorded` on the first reading within `DERIVED_SLACK_MS` of a `known` value (not on baseline settle), and derive the runner from the highest known cumulative rather than the highest re-read one. Proof: today 2026-09-25-vod goes 16 → 20 via the harness's own rebuild; after finding 16 lands, the day needs `audit --restart-at 12404` (thread the existing `replay(.., restart_at)` to the CLI) to keep exercising it.

## 7. The baseline frozen when the runner reaches a row is the last settled reading, not the most-read one: the comparison is then a "change" and gets filed
`src/marathon.rs:1243`
While a row is under the runner every pass overwrites `slot.baseline` with that pass's reading if its vote is settled, so a misread twice in the last passes before arrival (3:45:47 x2 vs 1:45:47 x14) is what the row carries; the true comparison then arrives as a change, and once the total nears it — or the category row above still carries yesterday's baseline so the arithmetic vouches — it is filed with the comparison's segment. vod-2880305036 Steel Legion: filed 6347/843 (the 2026-09-18 PB split) eleven minutes before the comparison time, real finish 18:31 at 1:48:40 refused as already recorded; expected.txt enshrines it. 63 slots across 12 days freeze on a non-majority value; one filed wrong.
Severity: loses-rows-sometimes / loses-rows-sometimes.
**Fix:** at runner arrival freeze the most-voted settled baseline vote, and refuse a candidate equal to any settled `baseline_votes` entry of its own row while the total has not reached it. Proof: `audit-race.sh` on vod-2880305036 moves row 15 from `6347/843` to `~6520/1111` (`--update` after eyeballing); 2026-09-25-live/vod, 09-28-vod, vod-2877553462 stay unchanged.

## 8. A comparison is filed as the finish when its segment cell misreads twice and the row above loses its anchor: Uninvited 2026-09-28-vod
`src/marathon.rs:1274` (same_segment rule), `:2054` (anchor disabled by any settled vote above), `:1834` (Unanchored arm, no arithmetic), `:358-376` (one-digit guard passes zero-differing) (merged: two findings)
Uninvited's comparison 12:59/1:26:37 read `12:39`/1:26:37 at t=5005 and 5305, so `same_segment` was false and the baseline's own cumulative became a candidate; at 5425 the category row above settled a garbage 5015 x2 (denied, unrecorded), `!any(settled)` at 2054 turned the anchor off, `segment_denies`/`board_vouches` fell silent, the Unanchored arm took 759 x2 with no arithmetic, `one_digit_from_baseline` passed a candidate equal to the baseline (zero chars differ), and the tie at 1367 chose the larger cumulative (5197 over the real 5193). Filed 5197/759 for 1:26:33/12:57; the transition and Steel Legion (884 → derived 882) then propagate; the live capture of the same day got 5193/777.
Severity: loses-rows-sometimes / loses-rows-sometimes (original A); loses-rows-sometimes / refuted-as-stated (original B: the missing spread check was not the deciding factor).
**Fix:** at 2054 count only settled votes that are coherent and not refused by `segment_denies` when deciding to drop the baseline anchor, and make the one-digit guard treat a candidate equal to the baseline cumulative as at least as suspect as one digit off. Proof: `audit-race.sh` on 2026-09-28-vod moves row 13 to `5193/777` and row 15 to `6120/884`, matching 2026-09-28-live.

## 9. A backfilled or closed cumulative is one the board never printed, so after a restart the exact `known` match fails and the game is filed twice
`src/marathon.rs:1681` (`below_cum − below_seg`), `:1543` (`total.max(sum)`), tested exactly at `:1149`, `:1405`, `:1686`
LiveSplit rounds each printed cell, so a derived cumulative is a second off (vod-2876668056 Parallel World backfilled 2:31:06, board prints 2:31:05/08); it goes to the DB as `last_timer_ms`, comes back through `seed()`, and `known.contains` is exact. Reproduced by splicing a let-go/re-take after t=9506 s: `dup=1`, two Parallel World REC lines 9066000 and 9065000; the echo check only warns and only after an hour.
Severity: loses-rows-sometimes / loses-rows-sometimes (adds a spurious 21st row rather than losing one).
**Fix:** compare against `known` with `DERIVED_SLACK_MS` tolerance at 1149/1405/1686 (or flag derived cumulatives approximate). Proof: vod-2876668056 with a restart spliced after 9506 s (or `--restart-at`) → `dup=0`; the plain sweep unchanged.

## 10. A sub-minute delta whose digit reads as a letter (`-$.6`) is never a cell, so the row being run votes on that pass
`src/board.rs:165` (`glyph_repair` requires a colon)
`time_cell("-$.6")` fails, `name_word` accepts it, the delta glues onto the name, `has_delta` sees two cells, and the runner's-row no-vote rule is bypassed on exactly the passes where he is within seconds of his comparison. On 2026-09-28-vod t=5305 "Uninvited -$.6" was the second vote that settled the comparison in finding 8; a counterfactual with `["-5.6","12:39","1:26:37"]` files 5193/777. Four such rows in the corpus (`-S.10`, `-2$.9`, `+258.4`).
Severity: loses-rows-sometimes / loses-rows-sometimes.
**Fix:** let `glyph_repair` accept a signed word with a point and a digit body (sign + `.`, not colon), keeping the bare `$`/`S` exclusion. Proof: 2026-09-28-vod row 13 → `5193/777` (the `+258.4` colon-dropped variant needs the finding-8 fix as well).

## 11. A marathon let go (three `Other` passes) or replaced (`disowns`) is dropped without `close()`, so a finish with one pass on the board is lost
`src/app.rs:1783` and `:1817`; mirrored in `src/marathon.rs:2621` (merged: three findings)
Only StreamOffline and end-of-input call `m.close()`; a finish needs the pass after the delta-moving one plus a second reading 45 s later, so the last game sits unfiled ~2 minutes, and if he loads a per-game practice pane in that window (classifies `Other`, `claims` matches nothing) the tracker is taken with the row at one vote. Reproduced on 2026-09-28-vod: truncate at pass 272 → close files row 39; the same 272 passes plus three Ninja Gaiden passes → row 39 lost. Not seen live (he lingered 12–44 min after Moon Crystal every day); one skeptic showed that on 2026-09-25-vod a close would not have filed it anyway (`latest` prefers the newest single misread).
Severity: loses-rows-sometimes / refuted (shape absent); loses-rows-sometimes ×2; loses-rows-sometimes ×2.
**Fix:** call `m.close(at_ms)` + `file_completions` on the taken state in both arms and in `replay`; close() already refuses a runner row whose sum is off the total. Proof: the truncated-plus-three-foreign-passes variant of 2026-09-28-vod files row 39; the full `audit-race.sh` sweep unchanged.

## 12. The board's title is only ever its category line; a race-day category without "big 20"/"practice run" makes the early fresh-splits board somebody else's
`src/board.rs:335` (`title_lines` needs ≥4 letters; "Big 20 #23" has three), `live.toml:298`
Every corpus pass is titled "Practice Run" (the category); if he retitles to "Race"/"Race #23" and runs fresh splits, the Run-shaped board (Titles labels, column under 20 min) is `Verdict::Other` until ~34 min, no board lock is granted and take-up waits. Replayed on vod-2876668056 with titles rewritten to "Race": take-up moves from t=466 s to 2846 s and Double Dragon II (slot 5) is lost (becomes a baseline), Crisis Force minted wrong; with comparisons loaded (vod-2881152722) nothing changes.
Severity: loses-rows-on-race-day / loses-rows-sometimes (needs both a retitle and no comparison).
**Fix:** widen `match` in `live.toml:298` with `"race"`, `"#23"`, `"big 20 #23"` (config only), and optionally let a header line with `#`+digits pass the four-letter bar. Proof: vod-2876668056 with `sed 's/Practice Run/Race/'` on titles keeps `5:2397/1749` in REC.

## 13. The board first shown after Die Hard loses Die Hard, live and in the replay
`src/marathon.rs:1159` (`chained` needs i>0), `:1185` (`fresh` false after one baseline vote), `:1207` (`just_now`)
On vod-2876668056 the game scene appeared at t=456 s with Die Hard 2:22/2:22 already shown; the lock frame's pass had `total_ms` None (first timer read 0.5 s later), so a baseline vote was cast, `fresh` is false for good, and slot 0 settles `Was(142000)`. On a comparison board `arriving` is false anyway, so race day loses Die Hard whenever the board scene follows an intro/countdown scene by more than ~1.5 min. (ninja-gaiden.db has that day's Die Hard only because the import predates the `fresh` guard; a re-import today would drop it.)
Severity: loses-rows-sometimes / loses-rows-sometimes. Overlaps finding 1 in loss, not in trigger.
**Fix:** a baseline vote cast on a pass with `total_ms == None` should not count against `fresh` (the lock and first timer read land one frame apart); the finder's cum==seg rule is unsafe on a comparison board (an unrun Die Hard's 2:22/2:22 would pass). Proof: `audit-race.sh` on vod-2876668056 gains `1:142/142`.

## 14. after-broadcast.sh takes a closed session for the end of the stream, and its VOD-finished test passes on a VOD still growing
`scripts/after-broadcast.sh:52` and `:71`; `scripts/list-vods.sh:49`
A mid-race restart closes the session for 12–17 s (sessions 244→245 on 09-25; six of nine practice days had such windows, 09-22 had an 8-minute offline gap) and the 60 s poll declares the stream over; list-vods prints length in 0.1 h buckets so two 120 s polls agree 2/3 of the time on a growing VOD (verified live: 6.6 h on three polls). ffmpeg then opens the EVENT playlist at the live edge, the pass is dated from the VOD's createdAt, closes normally, and the printed `--replace-live` deletes the morning's live rows inside its mis-dated span.
Severity: loses-rows-sometimes / loses-rows-sometimes.
**Fix:** require `open()==0` on three consecutive polls (longer than supervisor sleep + startup) and compare raw `lengthSeconds` over more than one bucket (or wait for `#EXT-X-ENDLIST`). Proof: the sessions table's 09-25 gap and a live-stream poll; not an audit-race.sh case. Side note the script also takes only today's newest VOD, so 09-22's two-VOD day never replays the morning.

## 15. `tracker-unvouched` clears itself after an hour whether or not the row was filed
`scripts/healthcheck.sh:318-326`
The signal is built from "nothing vouches for it" lines within the last hour, the tracker says that line once per row, and CLEAR is emitted when the window empties. 5 of 6 clears in health.log were expiry clears; on 2026-09-28 (Parallel World) and 2026-09-29 (Celeste) the phone said "all clear" for games still absent from the database, and on 09-24 the CLEAR went out four minutes before Kid Klown was actually filed.
Severity: cosmetic / loses-rows-sometimes (it loses the prompt to replay, not the row).
**Fix:** persist alerted rows in the state file and clear one only on a "filed after N unvouched passes" line or a "marathon over" line that does not list it, with no time window. Proof: replay the rule over `logs/live.log` 2026-09-28 — no CLEAR for Parallel World.

## 16. The audit harness rebuilds the tracker on ONE disowning pass; the loop needs three consecutive — 2026-09-25-vod's 16 is the harness's, not the bot's
`src/marathon.rs:2635` (`replay`, Board arm, no `else { hits = 0 }`; `app.rs:1889` has it) (merged: two findings)
After take-up `hits` stays ≥3 in `replay`, so the garbage pass at t=12404 ("Tr tm,") and the scroll-to-top at 15224 each rebuilt the tracker; the run loop's own pass over the same VOD (big20-db13/log-2883722634.txt) never rebuilt and filed 20. A two-line counterfactual build gives `2026-09-25-vod 20` with the four missing rows to the second and every other day byte-identical; audit-race.sh's header misattributes the gap, and the fixture tests drive `replay` too.
Severity: cosmetic / cosmetic (harness fidelity; no live row lost) — but it is the gate every other fix here is scored against.
**Fix:** add `else { hits = 0 }` after `state = Some(m)` in `replay`, run `audit-race.sh --update`, correct the header comment. Proof: 2026-09-25-vod reads 20 (`31:12326/822 33:12772/415 35:13549/747 39:14997/868` added).

## 17. `carry_hour` gives up on the first row past the hour, wasting most of its reads
`src/marathon.rs:1077` (`if prev < HOUR_MS { return }` before the own-comparison fallback at 1104)
Kid Klown (slot 8) is the first row past 1:00:00 every day and its finished cumulative reads hourless on 24/37, 7/16, 12/28 post-finish passes; each votes as mm:ss and is refused by `coherent`. Measured cost is a 2–5 minute settle delay (timestamps shift); a synthetic day with every hour-keeping read stripped loses Kid Klown, Excitebike and Uninvited (three games), which no corpus day approaches.
Severity: cosmetic / loses-rows-sometimes.
**Fix:** drop the line-1077 return; 1080-1083 already add the hour when `fixed < prev`, and the vote still faces coherent/denies/total. Proof: the `audit-race.sh` sweep must stay byte-identical (REC carries no timestamps); the hour-stripped variant of 2026-09-24-live is the unit test that gains rows 9/11/13.

## 18. A LiveSplit reset after a game was filed is invisible: recorded slots never vote again, the runner never moves up
`src/marathon.rs:1136`, `:1053`, `:829` (merged: two findings)
Doubling the first 4500 s of 2026-09-29-live files only the first copy's six rows; splicing 09-28 after 09-29's first 45 passes files the aborted attempt's Die Hard/Pac-Mania, Double Dragon II at the comparison, and loses Crisis Force (one skeptic showed two of those four are splice artefacts, so a real reset costs 2 rows for certain and a likely third). Zero occurrences in 19 captures; on racetime.gg a reset after filed games means abandoning the run, so this is a practice-day habit risk.
Severity: loses-rows-sometimes / loses-rows-sometimes (×2).
**Fix:** detect a recorded slot whose cumulative cell reads its `Was` baseline again with no delta for AGREE passes while the total is below its recorded cumulative, log "board reset" and rebuild unseeded. Proof: sweep unchanged (vod-2880305036's post-run reset at t=17618 must not change its 20/20); the doubled 2026-09-29-live synthetic files both copies.

## 19. No deploy runs after the session closes, so the last game and the race's final state reach the site at 23:50
`scripts/deploy-if-live.sh:7` (the finder's `:96` is wrong; the file is 8 lines), `crontab.example:16,18`
The last game is filed at the broadcast's end, always after the last ten-minute tick that could deploy, and the site-stale signal is gated on an open session; the now-card keys on `n.live` with no age check so it reads "Live · N of 20 games done" for up to nine hours. 09-24: deploys 14:50 then 23:50; he ends the stream a few minutes past the hour most days.
Severity: cosmetic / cosmetic (publication delay, nothing lost from the DB; on race day it is the final total that sits stale).
**Fix:** have `deploy-if-live.sh` also deploy when the newest hls session closed since the previous tick, and lift the site-stale gate to cover ten minutes after a close. Proof: `logs/deploy.log` after the next practice day shows a deploy within ten minutes of session close.

## 20. Nothing checks that the roster's placement ascends down the window, and `jb - ja` underflows in debug
`src/marathon.rs:2144-2200`, `:2176`
A swapped pair (Faria/Monster Party) reproduced in a scratch unit test refuses Faria (`coherent` against the recorded row below) and Parallel World (`segment_denies` off the wrong anchor). The corpus never shows two games out of order; the shape it does show is garbled bracketed rows placed on distant game slots (10 of 3,298 passes, absorbed as single unsettled votes), and `jb - ja` on those panics in a debug build (`cargo test` without `--release` on 2026-09-24-live pass 64).
Severity: loses-rows-sometimes / refuted as a race-day loss (cosmetic).
**Fix:** `jb.checked_sub(ja)` at 2176 plus a warn when placed slots are non-ascending (do not drop the placement — a real reorder would then record zero). Proof: sweep unchanged; a debug `cargo test` over the corpus stops panicking.

## 21. A day on which the total is never read files nothing at all
`src/marathon.rs:1917` (board_vouches refuses row 0), `:1882` (no total, no opinion), `:2058`, `:1159`
Replaying 2026-09-29-live with its obs withheld gives `0 of 39 rows`; the premise (race-day layout moves/covers the clock) has no evidence — the timer parsed on 58–97% of frames on all eleven race-board captures and a single late parse suffices to start the chain. Real residue: no health signal says "board lock, no total parsed", and `scripts/replay-big20-race.sh:78-81` still bakes the #101 crop (320,950,262×62) while `live.toml` moved to (376,952,236×52) in #124.
Severity: loses-rows-sometimes / refuted (none).
**Fix:** run `ngtwitchtimer locate` on a frame of the race-day scene the moment it is up and compare with `big20-race`'s rectangles; add a healthcheck signal for a board lock with no parsed total in 10 minutes. Proof: 2026-09-29-live with obs withheld is the alarm's test input.

## 22. `after-broadcast.sh --rollout` hides every way rollout.sh can fail and carries on
`scripts/after-broadcast.sh:58`
`set -u` only, output piped through a grep whose patterns match none of rollout.sh's refusals (`not main`, `uncommitted`, `tests failed`, `gave up`, `did not come back`), status never read; "rollout from main <sha>" is printed before rollout runs. Also `--dry-run` with `--rollout` is not dry (line 84 gates only the replay).
Severity: loses-rows-sometimes / cosmetic (live coverage, not stored rows).
**Fix:** `set -o pipefail` and check `${PIPESTATUS[0]}`, printing "rollout FAILED (exit N)"; add the refusal words to the pattern. Proof: `bash -n` and a deliberate run from a feature branch with the rollout stubbed.

---

## Refuted or reduced by the skeptics (what was considered)

- **`Verdict::Silent` leaves `hits`/`misses` untouched** (`app.rs:1773`) — mechanism real, but across ~3,500 in-force passes there are 2 disowning passes and 1 mid-run Other, never two bridged by Silents; every Silent-bridged let-go was after the last row. Cosmetic: the comment is wrong, nothing lost.
- **Segment accepted with two readings 10 s apart under Unanchored** (`marathon.rs:1834`) — no game row was ever recorded off a sub-45 s pair; the one wrong filing through that arm (Uninvited) had votes 300 s apart. Folded into finding 8 (the Unanchored arm's missing arithmetic is the real hole).
- **Audit harness cannot replay floors or a restart; baseline already disagrees with the binary** (`audit.rs:171`) — restart/floors gaps are real but no corpus day has a restart or a floor refusal; the "three diffs" were one diff, and it is exactly what #156 changed (`race-audit/` is gitignored; `--update` had not been run).
- **after-broadcast.sh fixes `today` at launch** — real, but the documented use is to launch during/after the broadcast; misuse replays yesterday into yesterday's directory and prints the date on the line the operator reads.
- **Rotated Twitch client-id blinds the import guard** — a rotated id 400s the stream resolve itself, so no session opens and no replay db can be produced; the confirmed half (list-vods stderr dropped, four-hour give-up with the wrong message) costs a delay. Worth noting the binary has no runtime client-id override.
- **Extra time column read as segment/cumulative** (`marathon.rs:2402`) — synthetic fourth column does collapse the day to 1/20, but the race board and the practice board are the same layout (live.toml:266) and no race-board pass in 19 captures has more than three cells.
- **After a restart nothing above the runner is `recorded`, so `close()` cannot file** (`marathon.rs:1517`) — the seed reconcile at 1149/1259 marks seeded rows recorded within two passes; restarts at the last 45 passes of both 09-24 captures file Moon Crystal. The adjacent real losses it surfaced are finding 6.
- **Scenarios handled as documented** (total past 5 h, PB as comparison, scrolling window, full names, breaks, chat off, skipped game) — confirmed handled; the second skeptic's re-discovery of Jaws is finding 2.

## Observations

- `race-audit/boards-2026-09-25-live.jsonl` is truncated at 163 of 285 passes (symlink to `replays/audit-0925/boards-today.jsonl`), so `audit-race.sh`'s "09-25 was rebuilt mid-run" header is wrong twice over: the live count is a partial capture, the vod count is finding 16.
- `NG_MARATHON_TRACE=<n>` takes a zero-indexed slot while every log line says "row n" one-indexed; several skeptics traced the wrong row first.
- The audit's key derivation prints "no roster fits / 0 of 0 played" and BAD for every row on the race board; only REC/SUM lines carry information there.
- Default (additive) `import-big20.sh` lets a duplicate Mega Man 6 through on 09-28 (same 665 s completion, starts 19 min apart, so the overlap dedupe misses it); a (game, category, final_time, day) match would catch it.
- `marathon.rs:1817` breaks segment ties toward the larger value and `:1814` in HashMap order (1 s nondeterminism between replays, e.g. New Ghostbusters II 771/772).
- `StreamOffline` has no debounce; a single GQL token refusal (`twitch_hls.rs:99`) runs the close in finding 5 mid-stream.
- `after-broadcast.sh --rollout` lands the rollout while `rollout.sh`'s wait is a no-op during a race (finding 4), so the "unattended race-day sequence" in docs/big20.md:44 is exactly the sequence that restarts the bot mid-run if started early.
