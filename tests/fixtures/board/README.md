# Board reader fixtures

Seven real LiveSplit panes as the two tesseract passes of `measure_pane`
saw them, with hand-checked ground truth, for the unit tests in
`src/board.rs`. Each JSON holds the crop (`crop`, canvas pixels of the
frame it was cut from), the OCR scale (`scale`, 2: the passes run on a 2x
grey upscale), the timer rectangle in 1x crop pixels (`timer`), the word
boxes of the digits-whitelisted pass (`words`) and the unrestricted pass
(`letters`) in 2x image pixels, and `expected`: title, subtitle, attempt
counter and the rows with their names and cells. A `notes` string on each
says what is odd about the frame.

Conventions of `expected.rows[].cells`: cells end with the segment and the
cumulative time; a printed numeric delta ("+0.1", "-21.2") is an extra
first cell; a bare "-" in the delta column (no comparison loaded) is not a
cell; "-" in a time column is kept; a cell cut off by the crop is null.
LiveSplit's true minus sign is written ASCII; so are apostrophes.

| fixture | frame | what it shows |
|---|---|---|
| `ng-default` | VOD 2862548640 at 8132 s (852x480), crop 250x272 at (52,192) | the default scene mid-run, Act 5 highlighted, deltas on the completed acts |
| `ng-theme` | VOD 2857064807 at 2300 s, crop 252x236 at (30,200) | the NES-styled scene, Act 3 highlighted, red deltas nearly invisible in grey |
| `jul14-opening` | the July 14 opening scene (frame 40), crop 250x240 at (30,196) | the pane drawn larger and cut at the right: title above the crop, cumulative column cut to "0:4" |
| `jul14-gameplay` | the July 14 gameplay scene (frame 70), crop 252x236 at (30,200) | a clean pane before the timer starts ("-5.00"), no highlight, no deltas |
| `arcathlon-final` | VOD 2858870362 at 15030 s (1080p source), crop 570x480 at (55,435) | "Randomized Arcathlon", ten games, the running one with "-" in both columns; the marathon timer is outside the crop, so the timer rectangle is placed under the last row |
| `arcathlon-numbered` | VOD 2830524439 at 5400 s, crop 255x275 at (28,193) | "Arcathlon #6" over background art, green deltas, a segment timer under the main one |
| `arcathlon-early` | VOD 2822281253 at 5400 s, crop 255x275 at (28,193) | three games done, one running, six "???" rows; the digits pass merges each row's two times |

The passes were run with the CLI arguments `ocr.rs` uses (`--dpi 96 --psm
11 -l eng`, the digits pass with `-c tessedit_char_whitelist=0123456789:.`
before the `tsv` config name — after it, tesseract ignores the whitelist).
Regenerating a fixture means re-running both passes on its PNG and
re-assembling the JSON around the same `expected`; the JSON key order is
name, source, scale, crop, timer, words, letters, expected, notes.
